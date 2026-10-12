-- The string library: every function with its edge cases.

local function show(...)
    local t = {}
    for i = 1, select('#', ...) do
        local v = select(i, ...)
        if type(v) == 'string' then
            t[i] = string.format('%q', v)
        else
            t[i] = tostring(v)
        end
    end
    print(table.concat(t, ' '))
end

local function try(f, ...)
    local ok, err = pcall(f, ...)
    if ok then return 'ok' else return 'error' end
end

print('len')
show(string.len(''), string.len('abc'), string.len('a\0b'), ('xyz'):len(), #'1234', string.len(123))

print('sub')
local s = 'hello'
for _, i in ipairs({-10, -6, -5, -1, 0, 1, 2, 5, 6, 10}) do
    show(i, s:sub(i), s:sub(i, 3), s:sub(2, i), s:sub(i, -2))
end
show(s:sub(3, 2), s:sub(0, 0), s:sub(6, 6), (''):sub(1), (''):sub(-1, 1), s:sub(2.7, 3.2))
show(s:sub('2', '3'), string.sub(12345, 2, 3))

print('upper lower reverse')
show(('Hello World 123'):upper(), ('Hello World 123'):lower(), ('abc'):reverse(), (''):reverse())
show(('\0a\255'):upper(), ('ÀÉ'):lower(), string.reverse(123))

print('rep')
show(('ab'):rep(3), ('ab'):rep(0), ('ab'):rep(-1), (''):rep(5), ('x'):rep(1), ('x'):rep(2.9))
show(string.rep(5, 3))

print('byte char')
show(('ABC'):byte(), ('ABC'):byte(2), ('ABC'):byte(-1), ('ABC'):byte(1, -1))
show(('ABC'):byte(0), ('ABC'):byte(10), ('ABC'):byte(2, 1), ('ABC'):byte(-10, 2), (''):byte())
show(('ABC'):byte(-3), ('ABC'):byte(-4), ('ABC'):byte(-5), ('ABC'):byte(-4, -4), ('ABC'):byte(-10))
show(string.char(), string.char(72, 105), string.char(0, 255), string.char('65', 66.9))
show(try(string.char, 256), try(string.char, -1), try(string.char, 'x'))

print('find')
show(('hello world'):find('o'), ('hello world'):find('o', 6), ('hello world'):find('o', -3))
show(('hello world'):find('xyz'), ('hello world'):find(''), ('hello world'):find('', 20))
show(('hello'):find('', 6), ('hello'):find('l', 0), ('hello'):find('l', -100))
show(('a.b'):find('.'), ('a.b'):find('.', 1, true), ('a.b'):find('%.'), ('a+b'):find('+', 1, true))
show(('hello'):find('l+'), ('hello'):find('(l)(l)'), ('hello'):find('()ll()'))
show(('hello'):find('^h'), ('hello'):find('^e'), ('hello'):find('o$'), ('hello'):find('l$'))
show(('x^y'):find('^', 1, true), ('x$y'):find('$y'), ('a\0b'):find('\0', 1, true), ('a\0b'):find('%z'))

print('match')
show(('hello world'):match('w%a+'), ('hello'):match('x'), ('key=value'):match('(%w+)=(%w+)'))
show(('  trim  '):match('^%s*(.-)%s*$'), ('2024-01-15'):match('(%d+)-(%d+)-(%d+)'))
show(('hello'):match('()ll()'), ('hello'):match('.-'), ('hello'):match('.*'), ('hello'):match('', 3))
show(('abc'):match('%a', 2), ('abc'):match('%a', -1), ('abc'):match('%a', 10))

print('classes')
local all = {}
for i = 0, 255 do all[#all + 1] = string.char(i) end
all = table.concat(all)
for _, c in ipairs({'%a', '%c', '%d', '%l', '%p', '%s', '%u', '%w', '%x', '%z', '.', '%A', '%D', '%S', '%W'}) do
    local n = select(2, all:gsub(c, ''))
    show(c, n)
end
show(('abc'):match('[%a]+'), ('a-b'):match('[a-c]+'), ('a]b'):match('[]]'), ('a^b'):match('[%^]'))
show(('abc123'):match('[^%d]+'), ('---'):match('[-]+'), ('x-y'):match('[x-]+'), ('z'):match('[a-]'))
show(('hello'):match('[%l]+'), ('HELLO'):match('[^%l]+'), ('a1_'):match('[%w_]+'))

print('quantifiers')
show(('aaa'):match('a*'), ('aaa'):match('a+'), ('aaa'):match('a-'), ('aaa'):match('a?'), ('baaa'):match('a*'))
show(('<a><b>'):match('<(.-)>'), ('<a><b>'):match('<(.*)>'), ('ab'):match('a?b'), ('b'):match('a?b'))
show(('aaab'):match('a-b'), ('xy'):match('x?y?z?'), ('123abc'):match('%d+%a+'), ('abc'):match('^(a)(b)(c)$'))

print('captures and back references')
show(('abcabc'):match('(abc)%1'), ('abcabd'):match('(abc)%1'), ('aa'):match('(a)%1'))
show(('xyzxyz'):find('(x(y)z)%1'), ('hello'):match('(h)(e)(l)(l)(o)'))
show(('"quoted" text'):match('(["\'])(.-)%1'), ('a.b.c'):match('(.-)%.(.*)'))

print('balance and frontier')
show(('x(a(b)c)y'):match('%b()'), ('(()'):match('%b()'), ('[[x]]'):match('%b[]'), ('aXb'):match('%bab'))
show(('THE (quick) fox'):find('%f[%a]%a+'), ('THE (quick) fox'):match('%f[%l]%a+'))
show(('hello world'):gsub('%f[%w]%w+', function(w) return w:upper() end))
show(('aaa bbb'):gsub('%f[%a]', '|'), ('aaa bbb'):gsub('%f[%A]', '|'))

print('gmatch')
for w in ('one two  three'):gmatch('%a+') do io.write(w, ',') end print()
for k, v in ('a=1, b=2, c=3'):gmatch('(%w+)=(%w+)') do io.write(k, v, ';') end print()
for p in ('abc'):gmatch('()') do io.write(p, ' ') end print()
for m in ('abc'):gmatch('%a*') do io.write('[', m, ']') end print()
for m in ('ab cd'):gmatch('.-') do io.write('[', m, ']') end print()
for m in ('xax'):gmatch('a*') do io.write('[', m, ']') end print()
for m in ('^a^a'):gmatch('^a') do io.write('[', m, ']') end print()

print('gsub')
show(('hello world'):gsub('o', '0'))
show(('hello world'):gsub('o', '0', 1))
show(('hello world'):gsub('o', '0', 0))
show(('hello world'):gsub('o', '0', -1))
show(('hello'):gsub('', '-'))
show(('abc'):gsub('%w*', '-'))
show(('abc'):gsub('b*', '-'))
show(('hello world'):gsub('(%w+)', '<%1>'))
show(('hello world'):gsub('%w+', '%0 %0'))
show(('abc'):gsub('(a)(b)(c)', '%3%2%1'))
show(('abc'):gsub('.', {a = 1, b = 'B'}), try(string.gsub, 'abc', '.', {b = true}))
show(('abc'):gsub('.', {a = 'x', b = false}))
show(('abc'):gsub('.', function(c) if c == 'b' then return nil end return c:upper() end))
show(#(('abc'):gsub('b', '%')), (('abc'):gsub('b', '%')) == 'a\0c', ('abc'):gsub('b', '%%'))
show(('abc'):gsub('b', '%x'), ('abc'):gsub('b', 'x%'):byte(1, -1))

print('format %s with zeros')
-- Lua 5.1 formats a string shorter than 100 bytes with sprintf, which stops at a zero byte.
show(#string.format('%s', 'a\0b'), #string.format('%5s', 'a\0b'), #string.format('%.1s', 'a\0b'))
show(#string.format('%s', ('a\0b'):rep(40)), #string.format('%-5s', ('a\0b'):rep(40)), #string.format('%.2s', ('a\0b'):rep(40)))
show(#string.format('%s', ('x'):rep(99) .. '\0y'), #string.format('%s', ('x'):rep(100) .. '\0y'))
show(('abc'):gsub('(.)()', function(c, p) return c .. p end))
show(('abc'):gsub('()', '%1'))
show(('a.b'):gsub('%.', '%%'))
show(('abc'):gsub('b', '%x'))
show(('hello'):gsub('^h', 'H'), ('hello'):gsub('^e', 'E'), ('hello'):gsub('o$', 'O'))
show(('  x  '):gsub('^%s+', ''), ('x'):gsub('x', 'a%%b'))
show(string.gsub(12345, 3, 'x'), ('abc'):gsub('', ''))
show(try(string.gsub, 'abc', '(', 'x'), try(string.gsub, 'abc', '%', 'x'), try(string.gsub, 'abc', '[a', 'x'))
show(try(string.gsub, 'abc', '.', {[1] = {}}), try(string.gsub, 'abc', '.', '%2'), try(string.gsub, 'abc', '%1', 'x'))

print('format')
show(string.format('%d %i %d %d', 42, -42, 3.9, -3.9))
show(string.format('%5d|%-5d|%05d|%+d|% d|%+5d', 42, 42, 42, 42, 42, -42))
show(string.format('%.3d|%.0d|%5.3d|%-+5d', 7, 0, 7, 7))
show(string.format('%u %o %x %X %#x %#o %#X', 42, 8, 255, 255, 255, 8, 255))
show(string.format('%x %u', -1, 2^53))
show(string.format('%c%c%c', 76, 117, 97), string.format('%5c|%-5c|', 65, 66))
show(string.format('%e %E %.2e %10.3e %-12.1e|', 12345.678, 0.000123, 1, -1.5, 2))
show(string.format('%f %.0f %.10f %8.2f %-8.2f| %08.2f %+.1f % f', 3.14159, 2.5, 1/3, -3.14159, 3.14159, 3.14159, 1, 1))
show(string.format('%g %g %g %g %g %g', 100000, 1000000, 0.0001, 0.00001, 1e100, 1/3))
show(string.format('%.3g %.10g %#g %#.3g %G %g', 1234.5678, 1/3, 1, 1, 1e-10, 0))
show(string.format('%f %e %g', 1/0, -1/0, 0/0 ~= 0/0 and 1 or 0))
show(string.format('%s %s %s', 'str', 42, 1.5), string.format('%5s|%-5s|%.2s|%5.1s|', 'ab', 'ab', 'abc', 'abc'))
show(string.format('%s', string.rep('x', 120)) == string.rep('x', 120), #string.format('%.99s', string.rep('x', 200)))
show(string.format('%q', 'a "quoted"\\ string\nwith\rcontrol\0chars'))
show(string.format('%q', 1/3), string.format('%q', 42))
show(string.format('%%d %%'), try(string.format, '%5%'))
show(try(string.format, '%3$s', 1))
show(try(string.format, '%d'), try(string.format, '%d', 'x'), try(string.format, '%y', 1))
show(try(string.format, '%100d', 1), try(string.format, '%.100f', 1), try(string.format, '%------d', 1))
show(try(string.format, '%099999d', 1), string.format('%-+ #0d', 5))
show(try(string.format, '%s %s', true, nil))
