-- Comments: `--[` opens a long comment only when `[` follows the `=`s, otherwise it is a short
-- comment, as in PUC-Rio Lua. A string with such a bracket is an error.

--[==== Section ====]
local a = 1
--[= not a long comment
local b = 2
--[ neither
local c = 3
--[[ a long
comment ]] local d = 4
--[==[ another ]] one
]==] local e = 5
assert(a == 1 and b == 2 and c == 3 and d == 4 and e == 5)

local f, err = load("return [==x")
assert(f == nil and err:find("long string"))
