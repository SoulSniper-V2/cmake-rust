set(l a;b;c;d)
list(TRANSFORM l TOUPPER AT 1 -1)
message(STATUS "l=${l}")
