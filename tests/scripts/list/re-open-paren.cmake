set(l x;y)
list(FILTER l INCLUDE REGEX [==[(]==])
message(STATUS "l=${l}")
