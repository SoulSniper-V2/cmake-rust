set(l foo.c;bar.h;baz.c)
list(TRANSFORM l TOUPPER REGEX "\\.c$")
message(STATUS "l=${l}")
