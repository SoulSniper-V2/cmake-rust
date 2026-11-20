set(l foo.c;bar.h;baz.c)
list(FILTER l EXCLUDE REGEX "\.c$")
message(STATUS "l=${l}")
