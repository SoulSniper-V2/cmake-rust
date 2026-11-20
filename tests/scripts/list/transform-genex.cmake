set(l "a;$<CONFIG:b>;$<1:c>")
list(TRANSFORM l GENEX_STRIP)
message(STATUS "l=${l}")
