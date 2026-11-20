set(l "hello world")
list(TRANSFORM l REPLACE "(h)(.*) (w)(.*)" "[\\4][\\2]")
message(STATUS "l=${l}")
