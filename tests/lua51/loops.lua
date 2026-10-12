-- Numeric and generic for loops as Lua 5.1 has them.

local function show(...)
    local t = {}
    for i = 1, select('#', ...) do
        t[i] = tostring((select(i, ...)))
    end
    print(table.concat(t, ' '))
end

print('zero steps')
-- A zero step runs no iteration when the start is below the limit, and otherwise never ends.
local n = 0 for i = 1, 10, 0 do n = n + 1 end show(n)
n = 0 for i = 1.5, 10, 0 do n = n + 1 end show(n)
n = 0 for i = 10, 1, 0 do n = n + 1 if n == 3 then break end end show(n)
n = 0 for i = 10.5, 1, 0 do n = n + 1 if n == 3 then break end end show(n)
n = 0 for i = 5, 5, 0 do n = n + 1 if n == 3 then break end end show(n)
n = 0 for i = 1, 3, 0/0 do n = n + 1 end show(n)
n = 0 for i = 1, 3, 1/0 do n = n + 1 end show(n)
n = 0 for i = 3, 1, -1/0 do n = n + 1 end show(n)

print('generic for')
-- Only nil ends the loop: false is passed on.
local values = {false, 1, false, nil, 2}
local i = 0
for v in function() i = i + 1 return values[i] end do show(i, v) end
for k, v in next, {false} do show(k, v) end
