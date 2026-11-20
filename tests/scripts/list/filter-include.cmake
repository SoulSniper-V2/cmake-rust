set(l foo.c;bar.h;baz.c)
list(FILTER l INCLUDE REGEX "\.c$")
message(STATUS "l=${l}")
