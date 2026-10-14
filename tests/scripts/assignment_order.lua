-- The order PUC-Rio Lua gives to the parts of local and multiple assignments.

-- A local statement brings its names into scope after all of its values.
do
    local a, b = 1, 2
    do
        local a, b = b, a
        assert(a == 2 and b == 1)
    end
    assert(a == 1 and b == 2)

    local x = 1
    local x, y = x + 1, x
    assert(x == 2 and y == 1)

    local function f() return 10, 20 end
    local p, q = 1
    local p, q, r = p, q, f()
    assert(p == 1 and q == nil and r == 10)
end

-- The table and key of each indexed target are evaluated before the values, from left to
-- right, and keep their value when the statement assigns the local they name.
do
    local i, t = 1, {}
    t[i], i = 10, 2
    assert(t[1] == 10 and t[2] == nil and i == 2)

    local j, a = 3, {}
    j, a[j] = j + 1, 20
    assert(a[3] == 20 and a[4] == nil and j == 4)

    local u = {}
    local old = u
    u[1], u = 1, {}
    assert(old[1] == 1 and u[1] == nil)

    local v = {}
    local old_v = v
    v, v[1] = {}, 1
    assert(old_v[1] == 1 and v[1] == nil)

    local w = {}
    local function through_upvalue()
        local old = w
        w[1], w = 1, {}
        assert(old[1] == 1 and w[1] == nil)
    end
    through_upvalue()

    local k = 1
    local function key_upvalue()
        local s = {}
        s[k], k = 10, 2
        assert(s[1] == 10 and s[2] == nil and k == 2)
    end
    key_upvalue()

    local order = {}
    local function f() order[#order + 1] = "f" return 1 end
    local function g() order[#order + 1] = "g" return 2 end
    local function h() order[#order + 1] = "h" return {} end
    local s, x = {}
    s[f()], x = 1, g()
    assert(table.concat(order, ",") == "f,g" and s[1] == 1 and x == 2)

    order = {}
    local y
    h()[f()], y = g(), f()
    assert(table.concat(order, ",") == "h,f,g,f")

    -- Many indexed targets with constant keys do not run out of registers.
    local big = {}
    big.a, big.b, big.c, big.d, big.e, big.f, big.g, big.h, big.i, big.j,
    big.k, big.l, big.m, big.n, big.o, big.p, big.q, big.r, big.s, big.t =
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20
    assert(big.a == 1 and big.t == 20)
end

-- An upvalue used as a table or key is read before the values too (PUC reads it into a
-- register when it parses the target), unless it is a table with a constant string key, which
-- Lua 5.4 stores through the upvalue (`SETTABUP`) after the values.
do
    local k = "a"
    local function f() k = "b" return 1 end
    local t = {}
    local function key_upvalue()
        local y
        t[k], y = f(), 2
    end
    key_upvalue()
    assert(t.a == 1 and t.b == nil)

    local u = {}
    local old = u
    local function h() u = {} return 1 end
    local function table_upvalue()
        local y
        u[1], y = h(), 2
    end
    table_upvalue()
    assert(old[1] == 1 and u[1] == nil)

    local w = {}
    local old_w = w
    local function hw() w = {} return 1 end
    local i = 1
    local function single_target()
        w[i] = hw()
    end
    single_target()
    assert(old_w[1] == 1 and w[1] == nil)

    local s = {}
    local new_s
    local function hs() new_s = {} s = new_s return 1 end
    local function string_key()
        s.x = hs()
    end
    string_key()
    assert(new_s.x == 1)
end
