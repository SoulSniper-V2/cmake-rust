add_executable(own IMPORTED)
set_property(TARGET own PROPERTY IMPORTED_LOCATION /usr/bin/true)
add_executable(own_alias ALIAS own)
add_custom_command(OUTPUT ${CMAKE_BINARY_DIR}/inc/gen.h
  COMMAND own -o ${CMAKE_BINARY_DIR}/inc/gen.h
  COMMAND own_alias x
  COMMAND early x
  COMMAND late x
  COMMAND sibtool x)
add_executable(app main.c ${CMAKE_BINARY_DIR}/inc/gen.h)
