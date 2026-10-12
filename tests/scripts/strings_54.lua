-- The string library in the default mode follows Lua 5.4 (also passes under PUC-Rio Lua 5.4.9).

-- Methods on strings.
assert(('abc'):upper() == 'ABC' and ('x'):rep(3) == 'xxx' and #('abc'):rep(2, ', ') == 8)
assert(('x'):rep(3, ',') == 'x,x,x' and ('x'):rep(0, ',') == '' and ('x'):rep(-1) == '')
assert(getmetatable('').__index == string)

-- Integer arguments must have an integer representation.
assert(not pcall(string.sub, 'abc', 1.5))
assert(('abc'):sub(2.0) == 'bc' and ('abc'):sub('2') == 'bc')
assert(not pcall(string.rep, 'x', 2.5))
assert(not pcall(string.char, 256) and not pcall(string.char, -1))

-- Positions.
assert(('hello'):find('l', 10) == nil and ('hello'):find('', 6) == 6 and ('hello'):find('', 7) == nil)
assert(('hello'):sub(math.mininteger, math.maxinteger) == 'hello')
assert(('hello'):byte(-100, 100) == 104)
local t = {}
for w in ('one two three'):gmatch('%a+', 5) do t[#t + 1] = w end
assert(#t == 2 and t[1] == 'two')

-- Empty matches.
assert(select(1, ('abc'):gsub('%w*', '-')) == '-')
assert(select(1, ('abc'):gsub('b*', '-')) == '-a-c-')
local n = 0
for m in ('abc'):gmatch('%a*') do n = n + 1 end
assert(n == 1)

-- '%g' and '%z'.
assert(('a b'):gsub('%g', '') == ' ')
assert(('a\0b'):find('%z') == 2)

-- Replacement strings.
assert(not pcall(string.gsub, 'abc', 'b', '%x'))
assert(('abc'):gsub('b', '%%') == 'a%c')
assert(('abc'):gsub('(b)', '%1%1') == 'abbc')
assert(not pcall(string.gsub, 'abc', '.', {a = true}))

-- Patterns.
assert(('THE (quick) fox'):match('%f[%a]%a+', 2) == 'quick')
assert(select(2, ('x'):rep(100):gsub('.', '')) == 100)
assert(not pcall(string.find, 'x', ('('):rep(40) .. 'x' .. (')'):rep(40)))
assert(not pcall(string.match, 'x', '%'))
assert(not pcall(string.match, 'x', '[x'))
assert(not pcall(string.match, 'x', '(x'))
assert(not pcall(string.match, 'x', 'x)'))
assert(not pcall(string.gsub, 'alo', '.', '%2'))

-- Formatting.
assert(string.format('%5.2s|', ('x'):rep(2^10)) == '   xx|')
assert(not pcall(string.format, '%099999d', 1))
assert(not pcall(string.format, '%d', 2^63))
assert(string.format('%d', 3.0) == '3' and not pcall(string.format, '%d', 3.5))
assert(string.format('%q', 'a\0b\1c\n') == '"a\\0b\\1c\\\n"')
assert(string.format('%q', 1) == '1' and string.format('%q', math.mininteger) == '0x8000000000000000')
assert(string.format('%q', 0.5) == '0x1p-1' and string.format('%q', 1/0) == '1e9999')
assert(string.format('%a', 1) == '0x1p+0')
assert(not pcall(string.format, '%q', {}))
assert(not pcall(string.format, '%10q', 'x'))
assert(not pcall(string.format, '%#d', 1))
local mt = {__tostring = function() return 'custom' end}
assert(string.format('%s|%5s', setmetatable({}, mt), setmetatable({}, mt)) == 'custom|custom')
assert(string.format('%s %s', true, nil) == 'true nil')
assert(string.format('%x', -1) == 'ffffffffffffffff')
assert(string.format('%5.1f|%-8.3e|%g', 3.14159, 0.5, 1e20) == '  3.1|5.000e-01|1e+20')

-- `string.format` calls `__tostring` for '%s' arguments in order.
do
    local calls = {}
    local function obj(name)
        return setmetatable({}, {__tostring = function() calls[#calls + 1] = name return name end})
    end
    assert(string.format('%d %s %s %%s %5s', 1, obj('a'), obj('b'), obj('c')) == '1 a b %s     c')
    assert(table.concat(calls, ',') == 'a,b,c')
end

-- Repetitions too long to allocate are errors, also without limits.
assert(not pcall(string.rep, 'aa', math.maxinteger // 2 + 10))
assert(not pcall(string.rep, 'x', math.maxinteger))

-- Floats convert to strings with "%.14g", and ".0" when they look like integers.
assert(tostring(0.1 + 0.2) == '0.3' and tostring(1/3) == '0.33333333333333')
assert(tostring(1e15) == '1e+15' and tostring(2^53) == '9.007199254741e+15')
assert(tostring(3.0) == '3.0' and tostring(-2.0) == '-2.0' and tostring(-0.0) == '-0.0')
assert(tostring(1e100) == '1e+100' and tostring(123456.789e3) == '123456789.0')
assert(tostring(1/0) == 'inf' and tostring(-1/0) == '-inf')
assert(1.5 .. '' == '1.5' and 2^63 .. '' == '9.2233720368548e+18')
assert(string.format('%s|%d|%x|%5.1f', 2.0, 42, 255, 3.14159) == '2.0|42|ff|  3.1')
