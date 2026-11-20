set(l x;y)
list(FILTER l INCLUDE REGEX [==[[z-a]]==])
message(STATUS "l=${l}")
