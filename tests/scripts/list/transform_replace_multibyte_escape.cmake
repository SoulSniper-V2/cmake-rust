set(l a b)
list(TRANSFORM l REPLACE "a" "x\\éy")
message("[${l}]")
