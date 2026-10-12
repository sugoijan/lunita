-- math.random is PUC-Rio Lua 5.4's xoshiro256**, so a seed gives the same sequence (these values
-- are from PUC-Rio Lua 5.4.9).

math.randomseed(42)
local t = {}
for i = 1, 3 do t[#t + 1] = math.random() end
for i = 1, 3 do t[#t + 1] = math.random(100) end
for i = 1, 3 do t[#t + 1] = math.random(-5, 5) end
assert(t[4] == 86 and t[5] == 54 and t[6] == 64)
assert(t[7] == 1 and t[8] == -4 and t[9] == 3)
assert(math.random(0) == 3570341730643388674)
assert(math.random(math.mininteger, math.maxinteger) == -8801378756440956393)

math.randomseed(7, 9)
assert(math.random(1000) == 665)

-- Ranges.
for i = 1, 1000 do
    local f = math.random()
    assert(f >= 0 and f < 1)
    local n = math.random(3, 5)
    assert(n >= 3 and n <= 5)
    assert(math.random(1) == 1)
end
assert(not pcall(math.random, 2, 1))
assert(not pcall(math.random, 1, 2, 3))
assert(not pcall(math.random, 1.5))

-- `randomseed` returns the seed.
local a, b = math.randomseed(3)
assert(a == 3 and b == 0)
