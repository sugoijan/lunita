-- table.insert, table.remove and table.concat in the default mode follow Lua 5.4 (also passes
-- under PUC-Rio Lua 5.4.9).

local function errors(pattern, f, ...)
    local ok, err = pcall(f, ...)
    assert(not ok, 'expected an error')
    assert(string.find(err, pattern, 1, true), err)
end

-- Argument checks.
errors("wrong number of arguments to 'insert'", table.insert, {}, 1, 2, 3)
errors("wrong number of arguments to 'insert'", table.insert, {})
errors("position out of bounds", table.insert, {1, 2}, 4, 'x')
errors("position out of bounds", table.insert, {1, 2}, 0, 'x')
errors("position out of bounds", table.remove, {1, 2}, 4)
errors("position out of bounds", table.remove, {1, 2}, -1)
errors("number has no integer representation", table.insert, {}, 1.5, 'x')

-- Insert at the end, and remove past the end.
local t = {1, 2}
table.insert(t, 3, 'x')
assert(#t == 3 and t[3] == 'x')
assert(table.remove(t, 4) == nil and #t == 3)
assert(table.remove({}) == nil and select('#', table.remove({})) == 1)
assert(table.remove({}, 0) == nil)
t = {[0] = 'zero'}
assert(table.remove(t, 0) == 'zero' and t[0] == nil)

-- Elements in the hash part.
t = {}
for i = 10, 1, -1 do t[i] = i end
table.insert(t, 1, 0)
for i = 1, 11 do assert(t[i] == i - 1) end
assert(table.remove(t, 1) == 0)
for i = 1, 10 do assert(t[i] == i) end
assert(t[11] == nil)

-- Long shifts.
t = {}
for i = 1, 10000 do t[i] = i end
table.insert(t, 1, 0)
assert(#t == 10001 and t[1] == 0 and t[10001] == 10000)
assert(table.remove(t, 1) == 0 and #t == 10000 and t[10000] == 10000)

-- Metamethods: a proxy with `__index`, `__newindex` and `__len`.
local store = {}
local log = {}
local proxy = setmetatable({}, {
    __index = function(_, k) return store[k] end,
    __newindex = function(_, k, v) log[#log + 1] = k; store[k] = v end,
    __len = function() return #store end,
})
table.insert(proxy, 'a')
table.insert(proxy, 'b')
table.insert(proxy, 1, 'c')
assert(store[1] == 'c' and store[2] == 'a' and store[3] == 'b' and #store == 3)
assert(table.concat(log, ',') == '1,2,3,2,1')
assert(table.concat(proxy, '-') == 'c-a-b')
assert(table.concat(proxy, '-', 2) == 'a-b')
assert(table.remove(proxy, 1) == 'c')
assert(store[1] == 'a' and store[2] == 'b' and store[3] == nil)
assert(table.remove(proxy) == 'b' and #store == 1)

-- `__len` must return an integer.
local bad = setmetatable({}, {__len = function() return 'x' end})
errors("object length is not an integer", table.insert, bad, 1)

-- `__index` yielding inside a coroutine (PUC-Rio Lua cannot yield there).
if coroutine.yieldto then -- lunita only: PUC-Rio Lua cannot yield there
local yielding = setmetatable({}, {
    __index = function(_, k) coroutine.yield(k); return k * 10 end,
    __len = function() return 3 end,
})
local co = coroutine.create(function() return table.concat(yielding, ',') end)
for i = 1, 3 do assert(select(2, coroutine.resume(co)) == i) end
assert(select(2, coroutine.resume(co)) == '10,20,30')
end

-- Concat only takes strings and numbers, even with `__concat`.
local obj = setmetatable({}, {__concat = function(a, b)
    return (type(a) == 'table' and 'obj' or a) .. (type(b) == 'table' and 'obj' or b)
end})
errors("invalid value (table) at index 2 in table for 'concat'", table.concat, {1, obj, 3})
errors("invalid value (nil) at index 1 in table for 'concat'", table.concat, {}, ',', 1, 1)
-- Elements and the length go through `__index` and `__len`.
local proxy = setmetatable({}, {__index = function(_, i) return i * 10 end, __len = function() return 3 end})
assert(table.concat(proxy, ',') == '10,20,30')
errors("(string expected, got table)", table.concat, {1, 2}, {})
assert(table.concat({1, 2.5, 'x'}, ' ') == '1 2.5 x')
assert(table.concat({1, 2}, ',', 3, 2) == '' and table.concat({}) == '')

-- table.remove on an empty table returns and clears t[0], through the fast path too.
do
    local t = {[0] = 'x'}
    assert(table.remove(t) == 'x' and t[0] == nil and table.remove(t) == nil)
    local m = setmetatable({[0] = 'y'}, {__index = function() end})
    assert(table.remove(m) == 'y' and rawget(m, 0) == nil)
end

-- table.concat consults __len even with an end position given.
do
    local calls = 0
    local t = setmetatable({}, {__len = function() calls = calls + 1 return 0 end})
    assert(table.concat(t, ',', 1, 0) == '' and calls == 1)
    local ok, err = pcall(table.concat, setmetatable({}, {__len = function() error('boom', 0) end}), ',', 1, 0)
    assert(not ok and err == 'boom')
    assert(not pcall(table.concat, setmetatable({}, {__len = function() return 'x' end}), ',', 1, 0))
end
