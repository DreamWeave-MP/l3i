-- A small HTTPS client written in Luau over @dream/dns, @dream/tcp and @dream/tls: resolve,
-- race connections across the returned addresses, verify the server as the host that was
-- asked for, then one HTTP/1.1 exchange. A demonstration of the transport, not an HTTP
-- library: no redirects, no chunked bodies, no proxies, no connection reuse.
--
-- Every wait goes through one poller and is bounded by the call's deadline; nothing spins.
--
--   local https = HttpsClient
--   local secure = assert(https.connect('example.com', 443, { timeoutMs = 10000 }))
--   local response = assert(https.request(secure, 'GET', 'example.com', '/', { timeoutMs = 10000 }))
--   print(response.status, #response.body)

local dns = require('@dream/dns')
local tcp = require('@dream/tcp')
local tls = require('@dream/tls')

local HttpsClient = {}

local function remainingMs(deadline: number): number
	return math.max(0, math.floor((deadline - os.clock()) * 1000))
end

local function drain(poller)
	while poller:next() do
	end
end

-- Alternates address families, starting with the family the resolver put first (RFC 8305's
-- interleaving), so one unreachable family cannot hold the other back.
function HttpsClient.interleave(addresses: { string }): { string }
	local v6, v4 = {}, {}
	for _, address in addresses do
		table.insert(if string.sub(address, 1, 1) == '[' then v6 else v4, address)
	end
	local first, second = v6, v4
	if string.sub(addresses[1], 1, 1) ~= '[' then
		first, second = v4, v6
	end
	local ordered = {}
	for i = 1, math.max(#first, #second) do
		if first[i] then
			table.insert(ordered, first[i])
		end
		if second[i] then
			table.insert(ordered, second[i])
		end
	end
	return ordered
end

-- Resolves host, races TCP connections (a new attempt every attemptDelayMs, or at once when
-- one fails; the first to connect wins and the rest are closed), then completes a verified TLS
-- handshake for host. Returns the TLS stream and the endpoint that won, or nil, a message and
-- a kind.
function HttpsClient.connect(host: string, port: number, options: { [string]: any }?)
	local opts = options or {}
	local deadline = os.clock() + (opts.timeoutMs or 10000) / 1000
	local attemptDelay = (opts.attemptDelayMs or 250) / 1000
	local poller = tcp.poller({ maxWaitMs = 1000 })

	-- 1. Resolve, watched like any socket.
	local request, message, kind = dns.resolve(host, port, { timeoutMs = math.max(1, remainingMs(deadline)) })
	if not request then
		poller:close()
		return nil, message, kind
	end
	assert(poller:watch(request, 0, 'read'))
	local addresses
	while true do
		local list, why, failure = request:take()
		if list then
			addresses = list
			break
		end
		if failure ~= 'wouldBlock' then
			poller:close()
			return nil, why, failure
		end
		poller:wait(math.min(1000, remainingMs(deadline)))
		drain(poller)
	end
	poller:unwatch(0)

	-- 2. Race connections.
	local order = HttpsClient.interleave(addresses)
	local attempts = {}
	local nextIndex, nextStart = 1, os.clock()
	local winner, endpoint
	local lastMessage, lastKind = 'no address could be tried', 'notFound'
	local function cleanup()
		for token, stream in attempts do
			poller:unwatch(token)
			stream:close()
		end
		attempts = {}
	end
	while not winner do
		local now = os.clock()
		if now >= deadline then
			cleanup()
			poller:close()
			return nil, `connecting to {host}:{port} timed out`, 'timedOut'
		end
		if nextIndex <= #order and (now >= nextStart or next(attempts) == nil) then
			local stream, why, failure = tcp.connect(order[nextIndex])
			if stream then
				attempts[nextIndex] = stream
				assert(poller:watch(stream, nextIndex, 'write'))
			else
				lastMessage, lastKind = why, failure
			end
			nextIndex += 1
			nextStart = now + attemptDelay
		end
		if next(attempts) == nil and nextIndex > #order then
			poller:close()
			return nil, lastMessage, lastKind
		end
		local untilNext = if nextIndex <= #order then math.min(nextStart, deadline) else deadline
		poller:wait(math.min(1000, math.max(0, math.floor((untilNext - os.clock()) * 1000))))
		while true do
			local token = poller:next()
			if token == nil then
				break
			end
			local stream = attempts[token]
			if stream and not winner then
				local ok, why, failure = stream:finishConnect()
				if ok then
					winner, endpoint = stream, order[token]
					attempts[token] = nil
					poller:unwatch(token)
				elseif ok == nil then
					lastMessage, lastKind = why, failure
					attempts[token] = nil
					poller:unwatch(token)
					stream:close()
					nextStart = os.clock() -- the next attempt starts at once
				end
			end
		end
	end
	cleanup()

	-- 3. Verify the host that was asked for, whichever address answered.
	local secure, why, failure = tls.client(winner, { serverName = host, config = opts.tlsConfig, alpn = opts.alpn or { 'http/1.1' } })
	if not secure then
		winner:close()
		poller:close()
		return nil, why, failure
	end
	assert(poller:watch(secure, 1, 'readwrite'))
	while true do
		local done, reason, problem = secure:handshake()
		if done then
			break
		end
		if done == nil then
			poller:close()
			return nil, reason, problem
		end
		if os.clock() >= deadline then
			poller:close()
			secure:close()
			return nil, `the TLS handshake with {host} timed out`, 'timedOut'
		end
		poller:wait(math.min(1000, remainingMs(deadline)))
		drain(poller)
	end
	poller:close()
	return secure, endpoint
end

-- Writes all of data, waiting for writability through poller; true or nil, message, kind.
local function sendAll(secure, poller, data: string, deadline: number)
	local sent = 0
	poller:modify(1, 'write')
	while sent < #data do
		local count, why, failure = secure:write(data, sent)
		if count then
			sent += count
		elseif failure ~= 'wouldBlock' then
			return nil, why, failure
		elseif os.clock() >= deadline then
			return nil, 'sending timed out', 'timedOut'
		else
			poller:wait(math.min(1000, remainingMs(deadline)))
			drain(poller)
		end
	end
	while true do
		local flushed, why, failure = secure:flush()
		if flushed then
			return true
		end
		if flushed == nil then
			return nil, why, failure
		end
		if os.clock() >= deadline then
			return nil, 'sending timed out', 'timedOut'
		end
		poller:wait(math.min(1000, remainingMs(deadline)))
		drain(poller)
	end
end

local function parseHead(head: string)
	local lines = string.split(head, '\r\n')
	local version, status, reason = string.match(lines[1], '^(HTTP/1%.[01]) (%d%d%d) ?(.*)$')
	if not version then
		return nil
	end
	local headers = {}
	for i = 2, #lines do
		local name, value = string.match(lines[i], '^([^:]+):%s*(.-)%s*$')
		if name then
			headers[string.lower(name)] = value
		end
	end
	return { status = tonumber(status), reason = reason, headers = headers }
end

-- One request with Connection: close; the response's body is read by Content-Length, or to
-- a clean end of stream. A response that ends without close_notify is an error, not a body.
function HttpsClient.request(secure, method: string, host: string, path: string, options: { [string]: any }?)
	local opts = options or {}
	local deadline = os.clock() + (opts.timeoutMs or 10000) / 1000
	local limit = opts.maxResponseBytes or 16 * 1024 * 1024
	local poller = tcp.poller({ maxWaitMs = 1000 })
	assert(poller:watch(secure, 1, 'write'))
	local request = `{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: l3i-example\r\n\r\n`
	local sent, why, failure = sendAll(secure, poller, request, deadline)
	if not sent then
		poller:close()
		return nil, why, failure
	end
	poller:modify(1, 'read')
	local scratch = buffer.create(65536)
	local received = {}
	local size = 0
	local response
	local bodyStart
	while true do
		local count, reason, problem = secure:readInto(scratch)
		if count == 0 then
			break
		elseif count then
			table.insert(received, buffer.readstring(scratch, 0, count))
			size += count
			if size > limit then
				poller:close()
				return nil, 'the response is larger than maxResponseBytes', 'limitReached'
			end
			if not response then
				local text = table.concat(received)
				received = { text }
				local ending = string.find(text, '\r\n\r\n', 1, true)
				if ending then
					response = parseHead(string.sub(text, 1, ending - 1))
					if not response then
						poller:close()
						return nil, 'the response is not HTTP/1.x', 'invalidInput'
					end
					bodyStart = ending + 4
				end
			end
			local length = response and tonumber(response.headers['content-length'])
			if length and size - bodyStart + 1 >= length then
				break
			end
		elseif problem ~= 'wouldBlock' then
			poller:close()
			return nil, reason, problem
		elseif os.clock() >= deadline then
			poller:close()
			return nil, 'the response timed out', 'timedOut'
		else
			poller:wait(math.min(1000, remainingMs(deadline)))
			drain(poller)
		end
	end
	poller:close()
	if not response then
		return nil, 'the connection ended before a response head', 'truncated'
	end
	local text = table.concat(received)
	local body = string.sub(text, bodyStart)
	local length = tonumber(response.headers['content-length'])
	if length then
		if #body < length then
			return nil, `the body ended after {#body} of {length} bytes`, 'truncated'
		end
		body = string.sub(body, 1, length)
	end
	response.body = body
	return response
end

-- Sends close_notify, flushes it within timeoutMs, and closes.
function HttpsClient.close(secure, timeoutMs: number?)
	local deadline = os.clock() + (timeoutMs or 1000) / 1000
	if secure.state == 'open' and secure:shutdownWrite() == false then
		local poller = tcp.poller()
		poller:watch(secure, 1, 'write')
		while secure:flush() == false and os.clock() < deadline do
			poller:wait(math.min(1000, remainingMs(deadline)))
			drain(poller)
		end
		poller:close()
	end
	secure:close()
end

return HttpsClient
