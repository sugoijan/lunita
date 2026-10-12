-- Integer `%` and `//` follow PUC-Rio Lua 5.4: they never overflow, and by zero they are errors.

local min, max = math.mininteger, math.maxinteger

do
    -- At run time.
    local a, b = min, -1
    assert(a % b == 0)
    assert(a // b == min)
    assert(max % -1 == 0)
    assert(max // -1 == -max)
    assert(math.type(a % b) == "integer")
end

do
    -- Folded as constants by the compiler.
    assert(math.mininteger % -1 == 0)
    assert(-9223372036854775807 - 1 == math.mininteger)
    assert((-9223372036854775807 - 1) % -1 == 0)
    assert((-9223372036854775807 - 1) // -1 == math.mininteger)
end

do
    -- Floor semantics for every sign combination.
    assert(7 // 2 == 3 and 7 % 2 == 1)
    assert(-7 // 2 == -4 and -7 % 2 == 1)
    assert(7 // -2 == -4 and 7 % -2 == -1)
    assert(-7 // -2 == 3 and -7 % -2 == -1)
    assert(max % max == 0 and (max - 1) % max == max - 1)
    assert(min % max == max - 1)
    assert(min // max == -2)
    assert(-1 % min == -1 and 1 % min == min + 1)
end

do
    -- Float modulus follows `luai_nummod`.
    assert(5.5 % math.huge == 5.5)
    assert(-5.5 % math.huge == math.huge)
    assert(5.5 % -math.huge == -math.huge)
    assert(-5.5 % -math.huge == -5.5)
    assert(5.25 % 2 == 1.25 and -5.25 % 2 == 0.75)
    assert(5.25 % -2 == -0.75)
    local nan = 1 % 0.0
    assert(nan ~= nan)
    assert(1 // 0.0 == math.huge and -1 // 0.0 == -math.huge)
end

do
    -- Integer division by zero is an error, at run time and when it could be folded.
    local zero = 0
    assert(not pcall(function() return 1 % zero end))
    assert(not pcall(function() return 1 // zero end))
    assert(not pcall(function() return 1 % 0 end))
    assert(not pcall(function() return 1 // 0 end))
    assert(not pcall(function() return "1" % "0" end))
    -- Floats are fine.
    assert(1 // 0.0 == math.huge)
end

do
    -- Floats only convert to integers when exactly representable.
    assert(math.tointeger(2^63) == nil)
    assert(math.tointeger(-2^63) == math.mininteger)
    assert(math.tointeger(2^53) == 9007199254740992)
    assert(math.tointeger(0.5) == nil)
    local t = {}
    t[2^63] = "float"
    t[math.maxinteger] = "integer"
    assert(t[2^63] == "float" and t[math.maxinteger] == "integer")
end

-- Integers and floats compare exactly, without rounding the integer to a float.
do
    local maxint, minint = math.maxinteger, math.mininteger
    assert(2.0^53 + 1.0 ~= (1 << 53) + 1 and (1 << 53) + 1 > 2.0^53)
    assert(not ((1 << 53) + 1 == 2.0^53) and (1 << 53) == 2.0^53)
    assert(maxint < minint * -1.0 and maxint < 2.0^63 and not (2.0^63 <= maxint))
    assert(minint == -2.0^63 and minint <= -2.0^63 and not (minint < -2.0^63))
    assert(maxint - 1 < maxint + 0.0 and not (maxint + 0.0 <= maxint - 1))
    assert(1 < 1.5 and -1 > -1.5 and 2 > 1.5 and not (1 < 0 / 0) and not (1 >= 0 / 0))
    assert(1 < math.huge and minint > -math.huge and 3 == 3.0 and -0.0 == 0)
    assert(rawequal(3, 3.0) and not rawequal((1 << 53) + 1, 2.0^53))
    local t = {}
    t[(1 << 53) + 1] = 'int'
    t[2.0^53] = 'float'
    assert(t[(1 << 53) + 1] == 'int' and t[1 << 53] == 'float')
end

-- Shifts: a negative count shifts the other way, 64 or more gives 0.
do
    local minint = math.mininteger
    assert(1 << -11 == 0 and 1 >> -3 == 8 and 1 << 64 == 0 and 1 << 63 == minint)
    assert(-1 >> 1 == math.maxinteger and -1 >> 63 == 1 and -1 << -64 == 0)
    assert(1 << minint == 0 and 5 >> minint == 0 and 8 >> 3 == 1 and 3.0 << 1 == 6)
    local a, b = 1, -11
    assert(a << b == 0 and a >> -b == 0 and 1 >> -64 == 0)
end

-- Integer results the constant folder leaves to run time (they differ in Lua 5.1, which only has
-- floats) are the same as folded.
do
    assert(math.type(-0) == 'integer' and 1 / -0 == math.huge and 1 / (0 * -1) == math.huge)
    assert(4611686018427387904 * 4 == 0 and 3037000500 * 3037000500 == -9223372036709301616)
    assert(9223372036854775807 + 1 == math.mininteger and -(-9223372036854775807 - 1) < 0)
    assert(9007199254740993 ~= 9007199254740992 and 9007199254740993 - 9007199254740992 == 1)
    assert(9007199254740993 % 2 == 1 and 9007199254740993 > 9007199254740992.0)
    assert(-9007199254740993 == -(1 << 53) - 1 and ~9007199254740993 == -9007199254740994)
end
