-- Number formatting (%.14g), tonumber, coercions and arithmetic.

local function show(...)
    local t = {}
    for i = 1, select('#', ...) do
        t[i] = tostring((select(i, ...)))
    end
    print(table.concat(t, ' '))
end

print('formatting')
show(1, -1, 0, 100, 1e14, 1e15, 1e16, 123456789012345, 1234567890123456)
show(0.1, 0.5, 1/3, 2/3, 1/7, -1/3, 1e-4, 1e-5, 1.5e-300, 5e-324)
show(2^53, 2^63, 2^64, -2^63, 1e308, 1e100, 12345.678, 99999999999999.5)
show(1/0, -1/0, 3.0, -3.0, 2.5, 1e300 * 1e10)
local z = 0
-- (`0 * -1` here would print 0: PUC 5.1 merges the constant -0 it folds to with the 0 above, see
-- 'negative zero' below.)
show(-z, z)
show(3 / 2, 7 / 2, 10 / 4, 1e15 / 10, 2^-1074)
print(1 .. '', 1.5 .. '', 1e15 .. '', 1/3 .. '', -z .. '')
print(10 .. 20, 1 .. 2 .. 3, 'x' .. 1/4)

print('tonumber')
show(tonumber('10'), tonumber('  10  '), tonumber('\t10\n'), tonumber('10x'), tonumber('x10'))
show(tonumber('0x10'), tonumber('0X1f'), tonumber('-0x10'), tonumber('0x'), tonumber('0xg'))
show(tonumber('1e2'), tonumber('1E2'), tonumber('1e+2'), tonumber('1e-2'), tonumber('1e'), tonumber('e1'))
show(tonumber('.5'), tonumber('5.'), tonumber('.'), tonumber(''), tonumber(' '), tonumber('-'))
show(tonumber('1e99999'), tonumber('-1e99999'), tonumber('1e-99999'))
show(tonumber('0xffffffffffffffffffff'), tonumber('99999999999999999999'))
show(tonumber('inf'), tonumber('-inf'), tonumber('nan') ~= tonumber('nan'), tonumber('infinity'))
show(tonumber('1 2'), tonumber('1\0'), tonumber('--1'), tonumber('+1'), tonumber('+-1'))
show(tonumber(10), tonumber(1.5), tonumber(nil), tonumber(true), tonumber({}))
show(tonumber('ff', 16), tonumber('FF', 16), tonumber('-ff', 16), tonumber('zz', 36), tonumber('10', 2))
show(tonumber('8', 8), tonumber('777', 8), tonumber('  11  ', 2), tonumber('1.5', 10), tonumber('', 10))
show(tonumber('0x10', 16), tonumber('ffffffffffffffffff', 16), tonumber(10, 16), tonumber('1e1', 16))
print((pcall(tonumber, '10', 1)))
print((pcall(tonumber, '10', 37)))

print('coercions')
show('10' + 1, '10' * '2', ' 3 ' - 1, '0x10' + 0, '1e1' + 0, '-2' ^ 2, -'2')
show(10 .. 20, 1.5 .. '', '3' + 0.5, '10' / '4', '7' % '3')
print((pcall(function() return 'a' + 1 end)))
print((pcall(function() return {} .. 'x' end)))

print('arithmetic')
show(7 % 3, -7 % 3, 7 % -3, -7 % -3, 5.5 % 2, -5.5 % 2, 5.5 % -2, 0 % 5)
show(1 % 0 ~= 1 % 0, 1 / 0, -1 / 0, 0 / 0 ~= 0 / 0, 5.5 % (1 / 0) ~= 5.5 % (1 / 0))
show(2^10, 2^0.5, (-8)^(1/3) ~= (-8)^(1/3), 10^-2, 2^63 == 2^63 + 1)
show(math.floor(3.7), math.floor(-3.7), math.ceil(3.2), math.ceil(-3.2), math.floor(-0.5))
show(1e15 + 0.5, 2^53 + 1, 0.1 + 0.2, 0.1 + 0.2 == 0.3, 100000000000000)
for i = 1, 3 do show(i, i / 2, i * 0.1) end
for i = 1, 2, 0.5 do show(i) end
for i = 3, 1, -1 do show(i) end

print('negative zero')
-- Each case in its own function: PUC 5.1 merges the constants 0 and -0 within a function.
;(function() show(-0, 1/(-0)) end)()
;(function() show(0*-1, 1/(0*-1)) end)()
;(function() show(-1*0, 1/(-1*0)) end)()
;(function() show(0*0, 1/(0*0)) end)()
;(function() show(-(0), 1/-(0)) end)()
;(function() local z, m = 0, -1 show(-z, 1/(-z), 1/(z*m), 1/(m*z), z*m) end)()
;(function() show(1/(5-5), 1/(-5+5), 1/(0-0)) end)()

print('past 2^53')
;(function() local a = 4611686018427387904 show(a*4, a*4 == 2^64, a*-4) end)()
;(function() show(9223372036854775807 + 1, (9223372036854775807 + 1) == 2^63) end)()
;(function() show(9007199254740993 == 9007199254740992, 9007199254740993 < 9007199254740992) end)()
;(function() show(9007199254740993 - 9007199254740992, 9007199254740993 % 2) end)()
;(function()
    local b = 9007199254740993
    show(b == 9007199254740992, b - 9007199254740992, b % 2, b + 1 - b)
end)()
;(function()
    local c = 9007199254740992
    show(c + 1 == c, c + 1 - c, (c + 1) % 2, c * 2 + 1 == c * 2)
end)()
;(function()
    local t = {}
    t[9007199254740993] = 1; t[9007199254740992] = 2
    local n = 0 for _ in pairs(t) do n = n + 1 end
    show(n, t[9007199254740992])
end)()
;(function() local h = 7 for i = 1, 40 do h = h * 31 + i end show(h, h == h + 1) end)()
;(function()
    local mi = -9007199254740992
    show(-mi, mi - 1 == mi, mi * mi, 3 * 3000000000000000)
end)()
;(function()
    show(1e15 * 10 + 1, 123456789 * 987654321, 123456789 * 987654321 * 10)
end)()
;(function()
    local x = 9007199254740991
    show(x + 1, x + 2 == x + 1, x + 3, -(x + 2) == -(x + 1))
end)()
;(function() show(-9223372036854775807 - 1, -(-9223372036854775807 - 1)) end)()

print('tonumber bases')
show(tonumber('0x10', 10.7), tonumber('10', '10'), tonumber('0x10', 16.9), tonumber('z', 36.5))
