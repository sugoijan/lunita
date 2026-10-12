-- error levels and values, pcall, xpcall, select.

local function show(...)
    local t = {}
    for i = 1, select('#', ...) do
        local v = select(i, ...)
        t[i] = type(v) == 'table' and 'table' or tostring(v)
    end
    print(table.concat(t, ' '))
end

print('error')
show(pcall(error, 'plain'))
show(pcall(error, 'level 0', 0))
show(pcall(function() error('level 1') end))
show(pcall(function() error('level 1', 1) end))
local function lower() error('level 2', 2) end
show(pcall(function() lower() end))
local function deeper() lower() end
show(pcall(function()
    deeper()
end))
local function level3() error('level 3', 3) end
local function middle() level3() end
show(pcall(function() middle() end))
show(pcall(function() error('too deep', 50) end))
show(pcall(function() error(42) end))
show(pcall(function() error(42, 2) end))
show(pcall(function() error({}) end))
show(pcall(function() error() end))
show(pcall(function() error(nil) end))
show(pcall(function() error(true) end))
local t = {}
show(select(2, pcall(error, t)) == t)

print('runtime errors')
show((pcall(function() local x = nil; return x.y end)))
show((pcall(function() return 1 + {} end)))
show((pcall(function() return #nil end)))
show((pcall(function() undefined_function() end)))
show(pcall(function() return 1, 2, 3 end))

print('xpcall')
show(xpcall(function() return 'ok', 2 end, print))
show(xpcall(function() error('failed') end, function(m) return 'handled: ' .. m end))
show(xpcall(function() error({code = 1}) end, function(e) return type(e) end))
show(xpcall(function() error('a') end, function(m) error('b') end))
show(xpcall(function() local x = nil; return x.y end, function(m) return 'caught' end))
show(xpcall(function(...) return select('#', ...) end, print, 1, 2, 3))
show(xpcall(function() error('x') end, function(m) return 1, 2 end))
show(pcall(xpcall, function() error('x') end, nil))

print('nested pcall')
local depth = 0
local function nest(n)
    if n == 0 then error('bottom') end
    depth = depth + 1
    local ok, err = pcall(nest, n - 1)
    error(err, 0)
end
show(pcall(nest, 100), depth)
show(pcall(pcall, error, 'x'))
show((pcall(pcall)))

print('select')
show(select('#'), select('#', nil), select('#', nil, nil), select('#', 1, 2, 3))
show(select(1, 'a', 'b', 'c'))
show(select(2, 'a', 'b', 'c'))
show(select(3, 'a', 'b', 'c'))
show(select(4, 'a', 'b', 'c'))
show(select(-1, 'a', 'b', 'c'))
show(select(-3, 'a', 'b', 'c'))
show((pcall(select, -4, 'a', 'b', 'c')), (pcall(select, 0, 'a')), (pcall(select, 'x')))
show(select(2.7, 'a', 'b', 'c'))
show(select('2', 'a', 'b', 'c'))

print('assert')
show(pcall(assert, true, 'unused'))
show(pcall(assert, 1, 2, 3))
show(pcall(assert, false))
show(pcall(assert, nil, 'message'))
show((pcall(assert, false, {})))

print('load')
-- Lua 5.1 loads strings with `loadstring`.
local loadstr = loadstring or load
show(loadstr('return 1 + 1')())
show(pcall(loadstr('error("in chunk")')))
show(pcall(loadstr('error("in chunk", 2)')))
show(pcall(loadstr('error("in chunk")', 'named')))
show(pcall(loadstr('error("in chunk")', '=named')))
show(pcall(loadstr('\n\nerror("on line 3")')))
show(pcall(loadstr('local x = 1 -- a long chunk that does not fit in the chunk id at all\nerror("x")')))
show(loadstr('syntax error here') == nil)
local parts, i = {'return ', '1 ', '+ 2'}, 0
show(load(function() i = i + 1 return parts[i] end)())

print('library errors')
-- Errors raised by library functions get the position of the Lua code calling them, unless they
-- are called from another library function (such as pcall). (PUC-Rio Lua names a function called
-- from pcall or as a metamethod '?', piccolo names it.)
show(pcall(function() string.rep() end))
show(pcall(function() local s = string.rep('x', {}) end))
show(pcall(function() assert(false) end))
show(pcall(function() assert(nil, 'message') end))
show(pcall(function() assert(false, 42) end))
show(pcall(function() assert(false, {}) end))
show(pcall(assert, false))
show(pcall(function() setmetatable(1, {}) end))
show(pcall(function() setmetatable({}, 1) end))
show(pcall(function() setmetatable({}) end))
show(pcall(function() table.insert({}, 1, 2, 3) end))
show(pcall(function() table.concat({1, {}}) end))
show(pcall(function() tostring() end))
show(pcall(function() type() end))
show(pcall(function() tonumber() end))
show(pcall(function() tonumber('1', 99) end))
show(pcall(function() collectgarbage('nope') end))
show(pcall(function() next({}, 'missing') end))
show(pcall(function() pairs() end))
show(pcall(function() ipairs(1) end))

print('tail calls, pcall and xpcall of values that cannot be called, assert bytes')
local function tail() return string.rep() end
show(pcall(tail))
local function tail_error() return error('tail', 1) end
show(pcall(tail_error))
show((pcall(nil)))
show(pcall(function() local r, e = pcall() return r end))
show(xpcall(nil, function(m) return 'handled' end))
show(xpcall(nil, function() error('again') end))
show(pcall(function() local r = xpcall(print) return r end))
local ok, err = pcall(assert, false, 'a\0b')
show(ok, #err, err)
show(pcall(function() local r = math.randomseed() return r end))
show(math.randomseed(3))
local loop = {}
setmetatable(loop, {__index = loop, __newindex = loop})
show(tostring(select(2, pcall(function() return loop.x end))):find('loop in gettable', 1, true) ~= nil)
show(tostring(select(2, pcall(function() loop.x = 1 end))):find('loop in settable', 1, true) ~= nil)
show(1e15, 2^53, 0.1 + 0.2, 3.0)
show(table.concat({1, 2.5, 'x'}, ' '))
show(pcall(table.concat, {1, {}, 3}))

print('tostring')
-- `tostring` is a level for `error`, as a C function.
local ts = setmetatable({}, {__tostring = function() error('x', 2) end})
show(pcall(function() return tostring(ts) end))
show(pcall(tostring, ts))
-- Its `__tostring` result is returned as it is, one value.
local n = setmetatable({}, {__tostring = function() return 42 end})
show(type(tostring(n)), tostring(n))
local tt = setmetatable({}, {__tostring = function() return 'a', 'b' end})
show(select('#', tostring(tt)), tostring(tt))
local z = setmetatable({}, {__tostring = function() end})
show(select('#', tostring(z)), tostring(z))
local tb = setmetatable({}, {__tostring = function() return true end})
show(tostring(tb))

print('chunk names')
for _, n in ipairs({51, 52, 53, 55, 70}) do
    local name = ('f'):rep(n)
    show(select(2, pcall(loadstr("error('x')", '@' .. name))))
    show(select(2, pcall(loadstr("error('x')", '=' .. name))))
end
