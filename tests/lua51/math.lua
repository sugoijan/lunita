-- The math library (except random, whose generator differs, and the functions Lua 5.4 dropped,
-- which piccolo leaves to the host), comparisons and integer-valued floats.

local function show(...)
    local t = {}
    for i = 1, select('#', ...) do
        t[i] = tostring((select(i, ...)))
    end
    print(table.concat(t, ' '))
end

local function isnan(x) return x ~= x end
-- (A -0.0 literal would be merged with the constant 0 by PUC-Rio Lua 5.1.)
local function negate(x) return -x end
local nz = negate(0)

print('constants')
show(math.pi, math.huge, -math.huge, math.huge == 1 / 0)

print('abs')
show(math.abs(-3), math.abs(3), math.abs(-2.5), math.abs(nz), math.abs(-2^63), math.abs('-4'))
show((pcall(math.abs)), (pcall(math.abs, 'x')), (pcall(math.abs, {})))

print('floor and ceil')
show(math.floor(3.7), math.floor(-3.7), math.floor(3), math.floor(-0.5), math.floor(2^60 + 0.5))
show(math.ceil(3.2), math.ceil(-3.2), math.ceil(3), math.ceil(-0.5), math.ceil('1.5'))
show(math.floor(1e300), math.ceil(-1e300), math.floor(1 / 0), math.ceil(-1 / 0))
show(isnan(math.floor(0 / 0)), math.floor(2^53), math.floor(-2^63), math.floor(2^63))

print('fmod and modf')
show(math.fmod(7, 3), math.fmod(-7, 3), math.fmod(7, -3), math.fmod(-7, -3), math.fmod(5.5, 2))
show(isnan(math.fmod(1, 0)), math.fmod(0, 1), math.fmod(1, 1 / 0), isnan(math.fmod(1 / 0, 1)))
show(math.fmod(-6, 2), math.fmod(6, -2), math.fmod(2^53, 3), math.fmod(-2^63, -1))
show(math.modf(3.7))
show(math.modf(-3.7))
show(math.modf(5))
show(math.modf(nz))
show(math.modf(1 / 0))
show(math.modf(-1 / 0))
show(isnan((math.modf(0 / 0))), isnan(select(2, math.modf(0 / 0))))

print('sqrt, exp, log')
show(math.sqrt(16), math.sqrt(2), isnan(math.sqrt(-1)), math.sqrt(1 / 0), math.sqrt(nz))
show(math.exp(0), math.exp(1), math.exp(-1), math.exp(710), math.exp(-1000))
show(math.log(1), math.log(math.exp(2)), math.log(0), isnan(math.log(-1)), math.log(1 / 0))
-- Lua 5.1 ignores the base.
show(math.log(8, 2), math.log(100, 10))
show(2 ^ 10, 2 ^ 0.5, (-2) ^ 3, isnan((-2) ^ 0.5), 0 ^ 0, 0 ^ -1, (1 / 0) ^ 0, 2 ^ 1024, 2 ^ -1080)
show((pcall(math.sqrt)), (pcall(math.exp, 'x')), math.sqrt('4'), math.exp('0'))

print('trigonometry')
show(math.sin(0), math.cos(0), math.tan(0), math.sin(math.pi / 2), math.cos(math.pi))
show(math.asin(1), math.acos(1), math.atan(1), isnan(math.asin(2)), math.atan(1 / 0))
-- Lua 5.1 ignores the second argument.
show(math.atan(1, -1), math.atan(-1, -1), math.atan(nz))
show(math.deg(math.pi), math.rad(180), math.deg(1), math.rad(1))

print('min and max')
show(math.min(3, 1, 2), math.max(3, 1, 2), math.min(1), math.max(-1.5, -2), math.min('2', 3))
show(math.min(1, 2.5), math.max(2^53, 2^53 + 1), math.max(nz, 0), math.min(0, nz))
show((pcall(math.min)), (pcall(math.max)), (pcall(math.min, 1, 'x')), math.max('10', 9))

print('comparisons')
show(1 < 2, 1 < 1.5, 2^53 < 2^53 + 1, nz == 0, 1 / 0 > 2^1023, 0 / 0 == 0 / 0, 'a' < 'b')
show('10' < '9', 'abc' < 'abd', '' < 'a', 'Z' < 'a', 'a\0b' < 'a\0c', 'a' < 'a\0')
show((pcall(function() return 1 < '2' end)), (pcall(function() return {} < {} end)))
show(1 == '1', '1' == 1, 2^63 == -2^63, math.huge == math.huge, -math.huge < math.huge)

print('integer-valued floats')
show(2^31, 2^32, 2^52 + 0.5, 2^53 + 1, -2^31 - 1, 1e15 + 0.5, 0.1 + 0.2, 100 / 3)

print('errors')
-- (Called from Lua functions, so that PUC-Rio Lua 5.1 knows their names.)
local function message(f) print(select(2, pcall(f))) end
message(function() math.abs() end)
message(function() math.floor('x') end)
message(function() math.min(1, {}) end)
message(function() math.random(0) end)
message(function() math.random(3, 1) end)
message(function() math.random(1, 2, 3) end)
message(function() math.fmod(1) end)
