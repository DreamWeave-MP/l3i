-- A small HTTPS server written in Luau over @dream/tcp and @dream/tls: TLS handshakes, HTTP/1.1
-- requests without bodies, fixed Content-Length responses, keep-alive up to a request limit,
-- and a cap on open connections. A demonstration, not a production server: no request bodies,
-- no chunked encoding, no idle timeouts beyond the TLS handshake deadline.
--
-- The TLS part is the point: every connection is watched through one poller, reads and writes
-- are partial, the stream is watched for writing only while a response is waiting, and a
-- connection ends with close_notify.
--
--   local server = HttpsServer.new('127.0.0.1:8443', config, function(method, path)
--       return 200, 'text/plain', 'hello\n'
--   end)
--   while true do server:step(250) end

local tcp = require('@dream/tcp')
local tls = require('@dream/tls')

local LISTENER = 0
local MAX_HEAD = 16 * 1024

local REASONS = { [200] = 'OK', [400] = 'Bad Request', [404] = 'Not Found', [413] = 'Content Too Large', [431] = 'Request Header Fields Too Large' }

local HttpsServer = {}
HttpsServer.__index = HttpsServer

-- config: a dream.tls.ServerConfig. respond(method, path) returns status, content type, body.
-- options: maxConnections (64), maxRequestsPerConnection (16), handshakeTimeoutMs (10000).
function HttpsServer.new(address: string, config, respond, options: { [string]: any }?)
	local opts = options or {}
	local maxConnections = opts.maxConnections or 64
	local listener, message, kind = tcp.listen(address, { backlog = 128, noDelay = true, maxStreams = maxConnections })
	if not listener then
		return nil, message, kind
	end
	local poller = tcp.poller({ maxEvents = 64, maxWatches = maxConnections + 1 })
	assert(poller:watch(listener, LISTENER, 'read'))
	return setmetatable({
		listener = listener,
		poller = poller,
		config = config,
		respond = respond,
		maxRequests = opts.maxRequestsPerConnection or 16,
		handshakeTimeoutMs = opts.handshakeTimeoutMs or 10000,
		connections = {},
		nextToken = 1,
		scratch = buffer.create(16384),
		served = 0,
		address = listener.localAddress,
	}, HttpsServer)
end

function HttpsServer:drop(token)
	local connection = self.connections[token]
	self.poller:unwatch(token)
	connection.stream:close()
	self.connections[token] = nil
end

function HttpsServer:acceptAll()
	while true do
		local raw = self.listener:accept()
		if not raw then
			return -- 'wouldBlock', or 'limitReached' until a connection closes
		end
		local secure = tls.server(raw, self.config, { handshakeTimeoutMs = self.handshakeTimeoutMs })
		if not secure then
			raw:close()
		else
			local token = self.nextToken
			self.nextToken += 1
			self.connections[token] = { stream = secure, head = '', out = nil, sent = 0, requests = 0, closing = false }
			assert(self.poller:watch(secure, token, 'read'))
		end
	end
end

local function response(status: number, contentType: string, body: string, keepAlive: boolean): string
	return `HTTP/1.1 {status} {REASONS[status] or 'Unknown'}\r\n`
		.. `Content-Type: {contentType}\r\nContent-Length: {#body}\r\n`
		.. `Connection: {if keepAlive then 'keep-alive' else 'close'}\r\n\r\n`
		.. body
end

-- Answers the request at the front of the head buffer, if a whole head is there.
function HttpsServer:handle(token, connection)
	local ending = string.find(connection.head, '\r\n\r\n', 1, true)
	if not ending then
		if #connection.head > MAX_HEAD then
			connection.closing = true
			connection.out = response(431, 'text/plain', 'request head too large\n', false)
		end
		return
	end
	local head = string.sub(connection.head, 1, ending - 1)
	connection.head = string.sub(connection.head, ending + 4)
	connection.requests += 1
	local method, path = string.match(head, '^(%u+) (%S+) HTTP/1%.[01]\r?\n?')
	local lowered = string.lower(head)
	local length = tonumber(string.match(lowered, '\r\ncontent%-length:%s*(%d+)'))
	local wantsClose = string.find(lowered, '\r\nconnection:%s*close') ~= nil
	local keepAlive = not wantsClose and connection.requests < self.maxRequests
	local status, contentType, body
	if not method then
		status, contentType, body, keepAlive = 400, 'text/plain', 'bad request\n', false
	elseif length and length > 0 then
		status, contentType, body, keepAlive = 413, 'text/plain', 'request bodies are not supported\n', false
	else
		status, contentType, body = self.respond(method, path)
	end
	connection.closing = not keepAlive
	connection.out = response(status, contentType, body, keepAlive)
	connection.sent = 0
end

-- Reads what has arrived; a whole request head starts a response.
function HttpsServer:readFrom(token, connection)
	while not connection.out do
		local count, _, kind = connection.stream:readInto(self.scratch)
		if count == nil then
			if kind ~= 'wouldBlock' then
				self:drop(token) -- a failed handshake, a reset, a truncated stream
			end
			return
		end
		if count == 0 then
			self:drop(token) -- the client's close_notify
			return
		end
		connection.head ..= buffer.readstring(self.scratch, 0, count)
		self:handle(token, connection)
	end
end

-- Sends what is left of the response; switches the watch to writing only while bytes wait.
function HttpsServer:writeTo(token, connection)
	local stream = connection.stream
	while connection.sent < #connection.out do
		local count, _, kind = stream:write(connection.out, connection.sent)
		if count == nil then
			if kind == 'wouldBlock' then
				self.poller:modify(token, 'write')
			else
				self:drop(token)
			end
			return
		end
		connection.sent += count
	end
	local flushed, _, kind = stream:flush()
	if flushed == nil then
		self:drop(token)
		return
	end
	if not flushed then
		self.poller:modify(token, 'write')
		return
	end
	self.served += 1
	connection.out = nil
	if connection.closing then
		local closed = stream:shutdownWrite()
		if closed ~= false then
			self:drop(token) -- flushed, or failed
		else
			connection.out = '' -- nothing left but close_notify to flush
			connection.sent = 0
			self.poller:modify(token, 'write')
		end
		return
	end
	self.poller:modify(token, 'read')
	-- A pipelined request may already be waiting in the head buffer.
	self:handle(token, connection)
	if connection.out then
		self:writeTo(token, connection)
	end
end

function HttpsServer:step(timeoutMs: number)
	self.poller:wait(timeoutMs)
	while true do
		local token = self.poller:next()
		if token == nil then
			break
		end
		if token == LISTENER then
			self:acceptAll()
		else
			local connection = self.connections[token]
			if connection then
				if connection.out then
					if connection.out == '' and connection.closing then
						local done = connection.stream:flush()
						if done ~= false then
							self:drop(token)
						end
					else
						self:writeTo(token, connection)
					end
				else
					self:readFrom(token, connection)
					if self.connections[token] and connection.out then
						self:writeTo(token, connection)
					end
				end
			end
		end
	end
end

function HttpsServer:close()
	for token in self.connections do
		self:drop(token)
	end
	self.poller:close()
	self.listener:close()
end

return HttpsServer
