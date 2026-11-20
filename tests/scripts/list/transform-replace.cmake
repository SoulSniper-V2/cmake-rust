set(l abc;aac)
list(TRANSFORM l REPLACE "a(.)" "X\\1")
message(STATUS "l=${l}")
