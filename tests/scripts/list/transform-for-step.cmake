set(l a;b;c;d;e)
list(TRANSFORM l TOUPPER FOR 0 4 2)
message(STATUS "l=${l}")
