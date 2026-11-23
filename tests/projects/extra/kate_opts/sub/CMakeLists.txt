add_library(subobj OBJECT subobj.c)
add_executable(subapp subapp.c $<TARGET_OBJECTS:subobj>)
add_custom_target(subcustom ALL COMMAND ${CMAKE_COMMAND} -E touch stamp BYPRODUCTS stamp)
