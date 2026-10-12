-- Table semantics around the map part: removing during traversal, re-adding, growth and length.

local function count(t)
    local n = 0
    for _ in pairs(t) do n = n + 1 end
    return n
end

do
    -- Clearing every field during a traversal is allowed.
    local t = {}
    for i = 1, 100 do t["k" .. i] = i end
    for i = 1, 50 do t[i * 2] = i end
    for k in pairs(t) do t[k] = nil end
    assert(next(t) == nil)
    assert(count(t) == 0)
end

do
    -- Clearing other fields during a traversal is allowed too.
    local t = {}
    for i = 1, 100 do t["k" .. i] = i end
    local seen = 0
    for k, v in pairs(t) do
        seen = seen + 1
        t["k" .. (101 - v)] = nil
    end
    assert(seen == 50 or seen == 51)
end

do
    -- Re-adding removed keys, with string and object keys.
    local t = {}
    local keys = {}
    for i = 1, 20 do keys[i] = {} end
    for i = 1, 20 do t[keys[i]] = i t["s" .. i] = i end
    for i = 1, 20, 2 do t[keys[i]] = nil t["s" .. i] = nil end
    assert(count(t) == 20)
    for i = 1, 20, 2 do t[keys[i]] = -i t["s" .. i] = -i end
    assert(count(t) == 40)
    for i = 1, 20 do
        local expected = i % 2 == 1 and -i or i
        assert(t[keys[i]] == expected and t["s" .. i] == expected)
    end
end

do
    -- Many insertions and removals, the map part is compacted as it grows.
    local t = {}
    for round = 1, 20 do
        for i = 1, 1000 do t["x" .. i] = round end
        for i = 1, 1000, 3 do t["x" .. i] = nil end
    end
    local n = 0
    for k, v in pairs(t) do
        n = n + 1
        assert(v == 20)
    end
    assert(n == 666)
end

do
    -- Keys move from the map part to the array part as it grows.
    local t = {}
    for i = 10, 1, -1 do t[i] = i end
    for i = 1, 10 do assert(t[i] == i) end
    assert(#t == 10)
    local n = 0
    for k, v in pairs(t) do n = n + 1 assert(k == v) end
    assert(n == 10)
    t[5] = nil
    assert(#t == 10 or #t == 4)
end

do
    -- Float keys with integer values are integer keys, NaN and nil are not keys.
    local t = {}
    t[1.0] = "a"
    t[2^53] = "b"
    assert(t[1] == "a" and t[2^53 | 0] == "b")
    assert(not pcall(function() t[0/0] = 1 end))
    assert(not pcall(function() t[nil] = 1 end))
    assert(t[0/0] == nil and t[nil] == nil)
    assert(rawget(t, 0/0) == nil)
end

do
    -- `next` with a key that is not in the table is an error.
    assert(not pcall(next, {}, "missing"))
    assert(next({}, nil) == nil)
    local t = {a = 1}
    assert(next(t, "a") == nil)
end

do
    -- Borders.
    local t = {1, 2, 3}
    t[5] = 5
    local n = #t
    assert(n == 3 or n == 5)
    t = {}
    for i = 1, 100 do t[i] = i end
    for i = 100, 51, -1 do t[i] = nil end
    assert(#t == 50)
    t = {n = 1}
    t[1], t[2], t[3] = 1, 2, 3
    assert(#t == 3)
end
