-- The math library in the default mode follows Lua 5.4 (also passes under PUC-Rio Lua 5.4.9).

local function errors(pattern, f, ...)
    local ok, err = pcall(f, ...)
    assert(not ok, 'expected an error')
    assert(string.find(err, pattern, 1, true), err)
end

local function eq(a, b)
    return math.type(a) == math.type(b) and (a == b or (a ~= a and b ~= b))
end

-- Integers stay integers.
assert(eq(math.abs(math.mininteger), math.mininteger) and eq(math.abs(-3), 3))
assert(eq(math.abs(-2.5), 2.5) and eq(math.abs('-4'), 4.0) and eq(math.floor('3'), 3))
assert(eq(math.fmod(7, 3), 1) and eq(math.fmod(-7, 3), -1) and eq(math.fmod(7, -3), 1))
assert(eq(math.fmod(math.mininteger, -1), 0) and eq(math.fmod(7.0, 3), 1.0))
assert(eq(math.fmod(1, 0.0), 0 / 0) and eq(math.fmod(-6.0, 2), -0.0))
errors("bad argument #2 to", math.fmod, 1, 0)
errors("(zero)", math.fmod, 1, 0)

-- modf
local function modf(x, ip, fp)
    local a, b = math.modf(x)
    assert(eq(a, ip) and eq(b, fp), tostring(x))
end
modf(3.7, 3, 3.7 - 3)
modf(-3.7, -3, -3.7 + 3)
modf(5, 5, 0.0)
modf(-0.0, 0, 0.0)
modf(1 / 0, 1 / 0, 0.0)
modf(-1 / 0, -1 / 0, 0.0)
modf(1e300, 1e300, 0.0)
modf(0 / 0, 0 / 0, 0 / 0)

-- floor and ceil
assert(eq(math.floor(3.7), 3) and eq(math.ceil(-3.2), -3) and eq(math.floor(-0.0), 0))
assert(eq(math.floor(2^70), 2^70) and eq(math.ceil(-2^70), -2^70))
assert(eq(math.floor(1 / 0), 1 / 0) and eq(math.floor(0 / 0), 0 / 0))
errors("number expected, got string", math.floor, 'x')
errors("number expected, got no value", math.ceil)

-- log and atan
assert(eq(math.log(8, 2), 3.0) and eq(math.log(100, 10), 2.0) and eq(math.log(1), 0.0))
assert(math.abs(math.log(27, 3) - 3) < 1e-15)
assert(eq(math.atan(1, -1), 3 * math.pi / 4) and eq(math.atan(1), math.pi / 4))

-- min and max
assert(eq(math.max(1, 2.5), 2.5) and eq(math.min(1, 2.5), 1) and eq(math.max(3), 3))
errors("value expected", math.max)
errors("value expected", math.min)
assert(not pcall(math.max, 1, {}))

-- tointeger, type, ult
assert(eq(math.tointeger(3.0), 3) and math.tointeger(3.5) == nil and math.tointeger('x') == nil)
errors("value expected", math.tointeger)
errors("value expected", math.type)
assert(math.type(1) == 'integer' and math.type(1.0) == 'float' and math.type('1') == nil)
assert(math.ult(1, -1) and not math.ult(-1, 1))

-- random
errors("interval is empty", math.random, 3, 1)
errors("wrong number of arguments", math.random, 1, 2, 3)
errors("number has no integer representation", math.random, 1.5)
for _ = 1, 100 do
    local r = math.random(3, 5)
    assert(r >= 3 and r <= 5 and math.type(r) == 'integer')
end
