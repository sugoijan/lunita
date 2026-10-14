-- The order of local statements and multiple assignments, and `--[=` comments.

print('local')
local a, b = 1, 2
do
    local a, b = b, a
    print(a, b)
end
local x = 1
local x, y = x + 1, x
print(x, y)

print('assign')
local i, t = 1, {}
t[i], i = 10, 2
print(t[1], t[2], i)
local j, s = 3, {}
j, s[j] = j + 1, 20
print(s[3], s[4], j)
local u = {}
local old = u
u[1], u = 1, {}
print(old[1], u[1])
local w = {}
local function through_upvalue()
    local old = w
    w[1], w = 1, {}
    print(old[1], w[1])
end
through_upvalue()
local k = 1
local function key_upvalue()
    local r = {}
    r[k], k = 10, 2
    print(r[1], r[2], k)
end
key_upvalue()
local order = {}
local function f() order[#order + 1] = 'f' return 1 end
local function g() order[#order + 1] = 'g' return 2 end
local function h() order[#order + 1] = 'h' return {} end
local q, z = {}
q[f()], z = 1, g()
print(table.concat(order, ','), q[1], z)
order = {}
h()[f()], z = g(), f()
print(table.concat(order, ','))

print('comments')
--[==== Section ====]
--[= short
--[ short
--[[ long
]] print('after long')

print('upvalues')
local k = 'a'
local function fk() k = 'b' return 1 end
local tk = {}
local function key_upvalue()
    local y
    tk[k], y = fk(), 2
end
key_upvalue()
print(tk.a, tk.b)
local u = {}
local old_u = u
local function hu() u = {} return 1 end
local function table_upvalue()
    local y
    u[1], y = hu(), 2
end
table_upvalue()
print(old_u[1], u[1])
local w = {}
local old_w = w
local function hw() w = {} return 1 end
local iw = 1
local function single_target()
    w[iw] = hw()
end
single_target()
print(old_w[1], w[1])
