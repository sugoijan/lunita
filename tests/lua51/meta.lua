-- Metamethods, iteration, coroutines and loops as Lua 5.1 has them.

local function show(...)
    local t = {}
    for i = 1, select('#', ...) do
        t[i] = tostring((select(i, ...)))
    end
    print(table.concat(t, ' '))
end

local function try(f, ...)
    local ok, err = pcall(f, ...)
    if ok then return 'ok', err end
    -- Without the position, and the function's name (PUC names one called by pcall '?').
    return 'error', (tostring(err):gsub('^.-:%d+: ', ''):gsub("to '[^']*'", "to '?'"))
end

print('__len')
show(#setmetatable({}, {__len = function() return 7 end}))
show(#setmetatable({1, 2}, {__len = 1}))

print('ipairs and pairs')
local n = 0
for i, v in ipairs(setmetatable({}, {__index = function(t, i) if i <= 3 then return i * 10 end end})) do n = n + 1 end
show(n)
local s = ''
for i, v in ipairs(setmetatable({1, 2}, {__index = function(t, i) if i == 3 then return 30 end end})) do s = s .. v .. ',' end
show(s)
n = 0
for k, v in pairs(setmetatable({}, {__pairs = function(t) return function(_, k) if not k then return 1, 'one' end end, t, nil end})) do n = n + 1 end
show(n)
show(select('#', pairs({})), select('#', ipairs({})), (select(3, ipairs({}))))
show(select('#', next({})), select('#', next({1}, 1)))

print('__eq, __lt, __le')
-- (VM errors keep lunita's wording: only whether they are raised is compared.)
local function f() return true end
local a, b = setmetatable({}, {__eq = f}), setmetatable({}, {__eq = f})
show(a == b, a == {}, setmetatable({}, {__eq = f}) == setmetatable({}, {__eq = function() return true end}))
show((try(function() return setmetatable({}, {__lt = f}) < 1 end)))
show((try(function() return setmetatable({}, {__lt = f}) < setmetatable({}, {__lt = function() return true end}) end)))
local lt = {__lt = function(x, y) return true end}
show(setmetatable({}, lt) < setmetatable({}, lt), setmetatable({}, lt) <= setmetatable({}, lt))
local lt2 = {__lt = function(x, y) return false end}
show(setmetatable({}, lt2) <= setmetatable({}, lt2))
show((try(function() return setmetatable({}, {__le = f}) <= setmetatable({}, {__le = function() end}) end)))

print('unary metamethods')
show(-setmetatable({}, {__unm = function(x, y) return type(y) end}))

print('__call')
show((try(setmetatable({}, {__call = setmetatable({}, {__call = function() return 'chain' end})}))))
show(try(setmetatable({}, {__call = function() return 'called' end})))

print('coroutines')
show(select('#', coroutine.running()), coroutine.running())
show(coroutine.resume(coroutine.create(function() return select('#', coroutine.running()), type(coroutine.running()) end)))
show(try(coroutine.create, string.len))
show(try(coroutine.create, setmetatable({}, {__call = function() end})))
local outer
outer = coroutine.create(function()
    return coroutine.resume(coroutine.create(function() return coroutine.status(outer) end))
end)
show(coroutine.resume(outer))

print('for loops')
n = 0 for i = 1, 2, 0 do n = n + 1 end show(n)
n = 0 for i = 0/0, 1 do n = n + 1 end show(n)
n = 0 for i = 1, 3, 1/0 do n = n + 1 end show(n)
n = 0 for i = 1, 0/0 do n = n + 1 end show(n)
n = 0 for i = 1, 1e16, 1e16 do n = n + 1 end show(n)
n = 0 for i = 1, 3 do n = n + 1 end show(n)

print('library arguments')
show(try(error, 'x', {}))
show(try(error, 'x', 1.5))
show(try(getmetatable))
show(try(rawget, {}))
show(try(rawset, {}, 1))
show(try(assert, false, nil))
show(tonumber('10', 16.5), tonumber('0x1p+'), tonumber('0x1p+2'))
show(string.format('%c', 0):len(), string.format('%c%c', 0, 65):len(), string.format('%5c', 0):len())
show(_VERSION)

print('5.1 metamethod details')
-- `__len` of a userdata gets nil as its second argument (a table's `__len` is ignored).
local proxy = newproxy(true)
getmetatable(proxy).__len = function(x, y) return select('#', x, y) .. tostring(y) end
show(#proxy)
-- `<=` uses a shared `__lt` when the `__le` values are not the same metamethod.
local shared_lt = function(a, b) return a.n < b.n end
local le1 = setmetatable({n = 1}, {__le = function() return 'le1' end, __lt = shared_lt})
local le2 = setmetatable({n = 2}, {__le = function() return 'le2' end, __lt = shared_lt})
local only_lt = setmetatable({n = 3}, {__lt = shared_lt})
show(try(function() return le1 <= le2, le2 <= le1 end))
show(try(function() return le1 <= only_lt, only_lt <= le1 end))
show((try(function() return le1 <= setmetatable({}, {}) end)))
-- `__index` chains of up to 99 tables are followed, 100 are an error.
local function chain(n)
    local t = {value = 'end'}
    for _ = 1, n do t = setmetatable({}, {__index = t}) end
    return t
end
show(chain(99).value, (try(function() return chain(100).value end)))
-- A generic for ends on nil only.
local values = {false, false, nil}
local i = 0
for v in function() i = i + 1 return values[i] end do show(i, v) end
