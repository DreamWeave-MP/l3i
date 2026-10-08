-- A small HTTP/1.1 server written in Luau over @dream/tcp, to show the TCP primitives carrying
-- a real protocol. It is a demonstration, not a production server: one request per
-- connection, no request bodies, no keep-alive, no chunked encoding, no TLS, no timeouts
-- beyond the caller's loop. What it does do correctly is the TCP part: a request head
-- arrives in however many reads it takes, a response leaves in however many writes the
-- socket accepts, every handle is drained until 'wouldBlock', and 'write' is watched only
-- while bytes are waiting.
--
--   local server = HttpServer.new('127.0.0.1:8080', function(method, path)
--       return 200, 'text/plain', 'hello\n'
--   end)
--   while true do server:step(250) end

local tcp = require('@dream/tcp')

local LISTENER = 0
local MAX_HEAD = 16 * 1024

local REASONS = {
	[200] = 'OK',
	[400] = 'Bad Request',
	[404] = 'Not Found',
	[405] = 'Method Not Allowed',
	[431] = 'Request Header Fields Too Large',
}

local function response(status: number, contentType: string, body: string): string
	return `HTTP/1.1 {status} {REASONS[status] or 'Unknown'}\r\n`
		.. `Content-Type: {contentType}\r\n`
		.. `Content-Length: {#body}\r\n`
		.. 'Connection: close\r\n\r\n'
		.. body
end

local HttpServer = {}
HttpServer.__index = HttpServer

-- Listens on address; respond(method, path) returns status, content type and body.
function HttpServer.new(address: string, respond: (string, string) -> (number, string, string))
	local listener, message, kind = tcp.listen(address, { backlog = 128, noDelay = true })
	if not listener then
		return nil, message, kind
	end
	local poller = tcp.poller({ maxEvents = 64 })
	assert(poller:watch(listener, LISTENER, 'read'))
	return setmetatable({
		listener = listener,
		poller = poller,
		respond = respond,
		connections = {},
		nextToken = 1,
		scratch = buffer.create(4096),
		served = 0,
		address = listener.localAddress,
	}, HttpServer)
end

function HttpServer:finish(token)
	local connection = self.connections[token]
	self.poller:unwatch(token)
	connection.stream:close()
	self.connections[token] = nil
end

function HttpServer:acceptAll()
	while true do
		local stream, peer, kind = self.listener:accept()
		if not stream then
			-- 'wouldBlock': drained. 'limitReached' or an OS refusal: try again next step.
			return
		end
		local token = self.nextToken
		self.nextToken += 1
		self.connections[token] = { stream = stream, head = '', out = nil, sent = 0, peer = peer }
		assert(self.poller:watch(stream, token, 'read'))
	end
end

-- Sends what is left of the response; the rest waits for writable readiness.
function HttpServer:writeResponse(token, connection)
	while connection.sent < #connection.out do
		local count, _, kind = connection.stream:write(connection.out, connection.sent)
		if count == nil then
			if kind ~= 'wouldBlock' then
				self:finish(token) -- the client went away
			end
			return
		end
		connection.sent += count
	end
	self.served += 1
	-- End of stream tells the client the response is complete, then the socket goes.
	connection.stream:shutdown('write')
	self:finish(token)
end

function HttpServer:respondTo(token, connection, status, contentType, body)
	connection.out = response(status, contentType, body)
	self.poller:modify(token, 'write')
	self:writeResponse(token, connection)
end

-- Gathers the request head across as many reads as it arrives in.
function HttpServer:readRequest(token, connection)
	while true do
		local count, _, kind = connection.stream:readInto(self.scratch)
		if count == nil and kind == 'wouldBlock' then
			return
		end
		if count == nil or count == 0 then
			self:finish(token) -- reset, or closed before a whole request
			return
		end
		connection.head ..= buffer.readstring(self.scratch, 0, count)
		local ending = string.find(connection.head, '\r\n\r\n', 1, true)
		if ending then
			local method, path = string.match(connection.head, '^(%u+) (%S+) HTTP/1%.[01]\r\n')
			if not method then
				return self:respondTo(token, connection, 400, 'text/plain', 'bad request\n')
			end
			return self:respondTo(token, connection, self.respond(method, path))
		end
		if #connection.head > MAX_HEAD then
			return self:respondTo(token, connection, 431, 'text/plain', 'request head too large\n')
		end
	end
end

-- Waits up to timeoutMs for readiness and handles everything that is ready.
function HttpServer:step(timeoutMs: number)
	self.poller:wait(timeoutMs)
	while true do
		local token, readable, writable = self.poller:next()
		if token == nil then
			break
		end
		if token == LISTENER then
			self:acceptAll()
		else
			local connection = self.connections[token]
			if connection then
				if connection.out then
					if writable then
						self:writeResponse(token, connection)
					end
				elseif readable then
					self:readRequest(token, connection)
				end
			end
		end
	end
end

function HttpServer:close()
	for token in self.connections do
		self:finish(token)
	end
	self.poller:close()
	self.listener:close()
end

return HttpServer
