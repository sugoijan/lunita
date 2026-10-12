-- Numeric for loops follow PUC-Rio Lua 5.4: integer loops have their iteration count computed up
-- front and never overflow.

local function count(a, b, c)
    local n = 0
    if c then
        for _ = a, b, c do n = n + 1 end
    else
        for _ = a, b do n = n + 1 end
    end
    return n
end

local min, max = math.mininteger, math.maxinteger

assert(count(1, 10) == 10)
assert(count(10, 1) == 0)
assert(count(10, 1, -1) == 10)
assert(count(1, 10, 3) == 4)
assert(count(max - 1, max) == 2)
assert(count(min, min + 2) == 3)
assert(count(min + 2, min, -1) == 3)
assert(count(max, max - 2, -1) == 3)
assert(count(max, min, min) == 2)
assert(count(min, max, max) == 3)
assert(count(1, math.huge, max // 2) == 3)
assert(count(1, -math.huge) == 0)
assert(count(-1, -math.huge, min // 2) == 2)
assert(count(1, 2.5) == 2)
assert(count(1, 0.5) == 0)
assert(count(3, 1.5, -1) == 2)

do
    -- The loop variable is a copy, assigning to it does not change the iteration.
    local n = 0
    for i = 1, 3 do
        i = i * 10
        n = n + 1
    end
    assert(n == 3)

    local last
    for i = max - 2, max do last = i end
    assert(last == max)
    assert(math.type(last) == "integer")
end

do
    -- Float loops.
    assert(count(1, 2, 0.5) == 3)
    assert(count(1.0, 3) == 3)
    assert(count(2^53 - 3, 2^53 - 1) == 3)
    local last
    for i = 1, 2, 0.25 do last = i end
    assert(last == 2 and math.type(last) == "float")
end

do
    -- A zero step is an error.
    assert(not pcall(count, 1, 10, 0))
    assert(not pcall(count, 1, 10, 0.0))
    assert(not pcall(count, 1, "x"))
end
