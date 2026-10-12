-- Metamethods, iteration, coroutines and library arguments as Lua 5.4 has them (passes under
-- PUC-Rio Lua 5.4.9 built with its 5.3 compatibility, the default).

local function f() return true end

-- `<=` falls back to `not __lt(b, a)`.
local lt = {__lt = function(x, y) return x.n < y.n end}
local one, two = setmetatable({n = 1}, lt), setmetatable({n = 2}, lt)
assert(one <= two and not (two <= one) and one <= one)
assert(not pcall(function() return setmetatable({}, {}) <= setmetatable({}, {}) end))

-- A unary metamethod gets its operand twice.
assert(-setmetatable({}, {__unm = function(x, y) return type(y) end}) == 'table')
assert(#setmetatable({}, {__len = function(x, y) return type(y) end}) == 'table')
assert(~setmetatable({}, {__bnot = function(x, y) return type(y) end}) == 'table')

-- Iterators return three values, next one nil at the end.
assert(select('#', pairs({})) == 3 and select('#', ipairs({})) == 3)
assert(select(3, ipairs({})) == 0 and select(3, pairs({})) == nil)
assert(select('#', next({})) == 1 and select('#', next({1}, 1)) == 1)

-- Library argument checks.
assert(not pcall(error, 'x', {}) and not pcall(error, 'x', 1.5))
local ok, err = pcall(error, 'x', 1.5)
assert(err:find('number has no integer representation', 1, true))
assert(not pcall(getmetatable) and not pcall(rawget, {}) and not pcall(rawset, {}, 1))
ok, err = pcall(assert, false, nil)
assert(not ok and err == nil)
ok, err = pcall(assert, false)
assert(err == 'assertion failed!')
assert(not pcall(coroutine.create, setmetatable({}, {__call = function() end})))
assert(type(coroutine.create(string.len)) == 'thread')
assert(tonumber('0x1p+') == nil and tonumber('0x1p+2') == 4 and not load('return 0x1p+'))
assert(_VERSION == 'Lua 5.4')

-- `__pairs` returns exactly three values.
assert(select('#', pairs(setmetatable({}, {__pairs = function() return 1 end}))) == 3)
assert(select('#', pairs(setmetatable({}, {__pairs = function() return 1, 2, 3, 4, 5 end}))) == 3)

-- A generic for ends on nil only: false is passed on.
local seen = {}
local values = {false, 1, false, nil}
local i = 0
for v in function() i = i + 1 return values[i] end do seen[#seen + 1] = tostring(v) end
assert(table.concat(seen, ',') == 'false,1,false')

-- `__index` chains of 2000 tables are followed, 2001 are an error.
local function chain(n)
    local t = {value = 'end'}
    for _ = 1, n do t = setmetatable({}, {__index = t}) end
    return t
end
assert(chain(2000).value == 'end')
assert(not pcall(function() return chain(2001).value end))

-- A `<const>` local cannot be assigned through an upvalue, on any reference.
assert(not load('local x <const> = 1; local function f() x = 2 end'))
assert(not load('local x <const> = 1; local function f() local y = x; x = 2 end'))
assert(not load('local x <const> = 1; local function f() return function() local y = x; x = 2 end end'))
