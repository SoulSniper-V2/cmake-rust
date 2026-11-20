set(l x;y)
list(FILTER l INCLUDE REGEX [==[a**]==])
message(STATUS "l=${l}")
