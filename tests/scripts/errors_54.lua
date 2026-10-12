-- error levels, xpcall and select in the default mode (also passes under PUC-Rio Lua 5.4.9, run
-- from the tests/scripts directory so that the chunk name matches).

local name = ...
local function at(line, msg)
    return name .. ':' .. line .. ': ' .. msg
end

local ok, err = pcall(error, 'plain')
assert(not ok and err == 'plain')
ok, err = pcall(function() error('level 1') end)
assert(err == at(11, 'level 1'))
local function lower() error('level 2', 2) end
ok, err = pcall(function() lower() end)
assert(err == at(14, 'level 2'))
ok, err = pcall(function() error('level 0', 0) end)
assert(err == 'level 0')
ok, err = pcall(function() error(42) end)
assert(err == 42)
ok, err = pcall(function() error() end)
assert(not ok and err == nil)

-- xpcall passes extra arguments, and requires a function handler.
assert(select(2, xpcall(function(...) return select('#', ...) end, print, 1, 2, 3)) == 3)
ok, err = xpcall(function() error('x', 0) end, function(m) return 'handled ' .. m end)
assert(not ok and err == 'handled x')
ok, err = xpcall(function() error('a') end, function(m) error('b') end)
assert(not ok)
assert(not pcall(xpcall, function() end))
assert(not pcall(xpcall, function() end, 1))

-- select.
assert(select('#') == 0 and select('#', nil, nil) == 2)
assert(select(-1, 'a', 'b') == 'b' and select(2, 'a', 'b', 'c') == 'b')
assert(not pcall(select, 0, 'a') and not pcall(select, -3, 'a', 'b'))
assert(not pcall(select, 1.5, 'a') and select(2.0, 'a', 'b') == 'b')
assert(select(math.maxinteger, 'a') == nil)
assert(not pcall(select, math.mininteger, 1))

-- load.
local f = load('error("x")', '=chunk')
ok, err = pcall(f)
assert(err == 'chunk:1: x')
ok, err = pcall(load('error("x")'))
assert(err == '[string "error("x")"]:1: x')
assert(load(function() return nil end))

-- Errors raised by library functions get the position of the Lua code calling them.
ok, err = pcall(function() string.rep() end)
assert(err == at(49, "bad argument #1 to 'rep' (string expected, got no value)"), err)
ok, err = pcall(function() assert(false) end)
assert(err == at(51, 'assertion failed!'))
-- (Other values given to assert are raised as they are.)
ok, err = pcall(function() assert(false, 'message') end)
assert(err == at(54, 'message') and select(2, pcall(assert, false, 42)) == 42)
ok, err = pcall(function() table.insert({}, 1, 2, 3) end)
assert(err == at(56, "wrong number of arguments to 'insert'"))
ok, err = pcall(function() local x = math.floor('x') end)
assert(err == at(58, "bad argument #1 to 'floor' (number expected, got string)"))
-- Called from pcall, there is no Lua code to locate.
ok, err = pcall(assert, false)
assert(err == 'assertion failed!')

-- rawlen takes strings, pairs and ipairs need an argument.
assert(rawlen('abc') == 3 and rawlen({1, 2}) == 2)
assert(select(2, pcall(rawlen, 1)) == "bad argument #1 to 'rawlen' (table or string expected, got number)")
assert(select(2, pcall(pairs)) == "bad argument #1 to 'pairs' (value expected)")
assert(select(2, pcall(ipairs)) == "bad argument #1 to 'ipairs' (value expected)")

-- A library function called in a tail call errors at the call, as the calling frame stays.
local function tail() return string.rep() end
ok, err = pcall(tail)
assert(err == at(71, "bad argument #1 to 'rep' (string expected, got no value)"))
local function tail_error() return error('tail', 1) end
ok, err = pcall(tail_error)
assert(err == at(74, 'tail'))
-- pcall and xpcall return the error of calling a value that cannot be called.
ok, err = pcall(nil)
assert(not ok and err ~= nil)
assert(select(2, pcall(pcall)) == "bad argument #1 to 'pcall' (value expected)")
ok, err = xpcall(nil, function(m) return 'handled' end)
assert(not ok and err == 'handled')
assert(select(2, xpcall(nil, function() error('again') end)) == 'error in error handling')
assert(select(2, pcall(xpcall, print)) == "bad argument #2 to 'xpcall' (function expected, got no value)")
-- assert keeps the bytes of its message.
assert(select(2, pcall(assert, false, 'a\0b\255')) == 'a\0b\255')
-- randomseed checks its arguments and returns the seeds.
assert(select(2, pcall(math.randomseed, 'x')):find("(number expected, got string)", 1, true))
assert(select('#', math.randomseed(1, 2)) == 2)
-- __index and __newindex chains end after 2000 links.
local loop = {}
setmetatable(loop, {__index = loop, __newindex = loop})
assert(tostring(select(2, pcall(function() return loop.x end))):find("'__index' chain too long; possible loop", 1, true))
assert(tostring(select(2, pcall(function() loop.x = 1 end))):find("'__newindex' chain too long; possible loop", 1, true))
local chain = setmetatable({}, {__index = setmetatable({}, {__index = {deep = 1}})})
assert(chain.deep == 1 and chain.missing == nil)

-- Messages of type errors name the type found.
ok, err = pcall(function() return setmetatable({}, 1) end)
assert(err:find("bad argument #2 to 'setmetatable' (nil or table expected, got number)", 1, true))
ok, err = pcall(function() return setmetatable({}) end)
assert(err:find("(nil or table expected, got no value)", 1, true))
ok, err = pcall(function() return string.format('%-10q', 'x') end)
assert(err:find("specifier '%q' cannot have modifiers", 1, true))

-- tostring is a level for error, as a C function, and its __tostring result must be a string.
local ts = setmetatable({}, {__tostring = function() error('x', 2) end})
ok, err = pcall(function() return tostring(ts) end)
assert(not ok and err == 'x')
local tn = setmetatable({}, {__tostring = function() return 42 end})
assert(type(tostring(tn)) == 'string' and tostring(tn) == '42')
local tt = setmetatable({}, {__tostring = function() return 'a', 'b' end})
assert(select('#', tostring(tt)) == 1 and tostring(tt) == 'a')
ok, err = pcall(function() return tostring(setmetatable({}, {__tostring = function() return {} end})) end)
assert(err == at(114, "'__tostring' must return a string"))
ok, err = pcall(function() return tostring(setmetatable({}, {__tostring = function() end})) end)
assert(err == at(116, "'__tostring' must return a string"))
