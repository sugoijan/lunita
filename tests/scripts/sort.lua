
function is_sorted(list)
    for i = 2, #list do
        if list[i - 1] > list[i] then
            return false
        end
    end
    return true
end

do
    local t = { }
    table.sort(t)
    assert(#t == 0)

    t = { 4, 9, 3, 1 }
    table.sort(t)
    assert(#t == 4 and is_sorted(t))
end

do
    local list = {}
    for i = 1, 100 do
        table.insert(list, math.random())
    end

    table.sort(list)
    assert(#list == 100 and is_sorted(list))
end

-- The argument checks of PUC's `sort`, without a position; the comparator is only checked when
-- there is something to sort (Lua 5.4).
do
    local ok, err = pcall(table.sort, nil)
    assert(not ok and err == "bad argument #1 to 'sort' (table expected, got nil)", err)
    ok, err = pcall(table.sort, { 2, 1 }, true)
    assert(not ok and err == "bad argument #2 to 'sort' (function expected, got boolean)", err)
    assert(pcall(table.sort, { 1 }, true))
end
