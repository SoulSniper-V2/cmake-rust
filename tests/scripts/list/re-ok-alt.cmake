set(l x;y)
list(FILTER l INCLUDE REGEX [==[a|b]==])
message(STATUS "l=${l}")
