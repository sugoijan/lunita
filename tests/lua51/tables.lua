-- table.concat, insert, remove, sort (and unpack, which is a global in 5.1).

local unpack = unpack or table.unpack

local function show(...)
    local t = {}
    for i = 1, select('#', ...) do
        local v = select(i, ...)
        t[i] = type(v) == 'table' and 'table' or tostring(v)
    end
    print(table.concat(t, ' '))
end

local function dump(t, n)
    local parts = {}
    for i = 1, n or #t do parts[i] = tostring(t[i]) end
    return '{' .. table.concat(parts, ',') .. '}'
end

print('concat')
show(table.concat({}), table.concat({1, 2, 3}), table.concat({1, 2, 3}, ', '), table.concat({'a', 'b'}, ''))
show(table.concat({1, 2, 3}, '-', 2), table.concat({1, 2, 3}, '-', 2, 3), table.concat({1, 2, 3}, '-', 3, 2))
show(table.concat({1.5, 2, 'x'}, ' '), table.concat({1, 2, 3}, '-', 1, 1), table.concat({}, 'x', 1, 0))
show((pcall(table.concat, {1, {}, 3})))
show((pcall(table.concat, {1, nil, 3}, '-', 1, 3)))

print('insert')
local t = {}
table.insert(t, 'a') table.insert(t, 'b') table.insert(t, 1, 'c') table.insert(t, 2, 'd')
show(dump(t), #t)
table.insert(t, #t + 1, 'e')
show(dump(t), #t)
show((pcall(table.insert, t, 1, 2, 3)))
show((pcall(table.insert, nil, 1)))

print('remove')
t = {1, 2, 3, 4, 5}
show(table.remove(t), dump(t))
show(table.remove(t, 1), dump(t))
show(table.remove(t, 2), dump(t))
show(table.remove({}), table.remove({}, 1))
t = {1, 2, 3}
show(table.remove(t, #t), dump(t))

print('sort')
t = {5, 2, 8, 1, 9, 3}
table.sort(t)
show(dump(t))
table.sort(t, function(a, b) return a > b end)
show(dump(t))
t = {'banana', 'apple', 'cherry', 'Apple'}
table.sort(t)
show(dump(t))
t = {3.5, 1, 2.25, -1, 0}
table.sort(t)
show(dump(t))
t = {}
for i = 1, 100 do t[i] = (i * 37) % 101 end
table.sort(t)
local sorted = true
for i = 2, 100 do if t[i - 1] > t[i] then sorted = false end end
show(sorted, t[1], t[100])
show((pcall(table.sort, {1, 'x', 2})))
show((pcall(table.sort, {3, 2, 1}, function(a, b) error('cmp') end)))
t = {{n = 2}, {n = 1}, {n = 3}}
table.sort(t, function(a, b) return a.n < b.n end)
show(t[1].n, t[2].n, t[3].n)
t = {1}
table.sort(t)
show(dump(t))
table.sort({})

print('unpack')
show(unpack({1, 2, 3}))
show(unpack({1, 2, 3}, 2))
show(unpack({1, 2, 3}, 2, 3))
show(unpack({1, 2, 3}, 3, 2))
show(select('#', unpack({}, 1, 3)))
show(unpack({1, nil, 3}))

print('length')
show(#{}, #{1, 2, 3}, #{1, 2, nil}, #{n = 1}, #'abc')
t = {}
for i = 1, 10 do t[i] = i end
t[11] = nil
show(#t)
t[5] = nil
show(#t == 10 or #t == 4)

print('next and pairs')
t = {10, 20, 30}
local sum = 0
for k, v in pairs(t) do sum = sum + k * v end
show(sum)
for i, v in ipairs({1, 2, nil, 4}) do io.write(i, ' ') end print()
show(next({}), type(next({1})))
show(rawget({a = 1}, 'a'), rawequal(t, t), rawequal(t, {}))
local r = {}
rawset(r, 'x', 1)
show(r.x)
