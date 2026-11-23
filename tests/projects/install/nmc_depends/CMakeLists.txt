cmake_minimum_required(VERSION 3.30)
project(nmc_depends C)
# Target dependencies unite all configurations.
add_library(dbg STATIC a.c)
add_library(rel SHARED a.c)
add_library(plug MODULE a.c)
add_library(mid STATIC a.c)
target_link_libraries(mid PUBLIC "$<$<CONFIG:Release>:rel>")
add_executable(app main.c)
target_link_libraries(app PRIVATE "$<$<CONFIG:Debug>:dbg>" mid)
add_dependencies(app plug)
add_custom_target(stamp COMMAND echo stamp)
add_dependencies(stamp app)
install(TARGETS app rel plug RUNTIME_DEPENDENCIES
  DIRECTORIES "$<$<CONFIG:Debug>:/opt/dbg>" "/opt/all"
  PRE_EXCLUDE_REGEXES "lib\"c\\.so"
  RUNTIME DESTINATION bin
  LIBRARY DESTINATION "lib/$<CONFIG>" COMPONENT deps CONFIGURATIONS Release Debug PERMISSIONS OWNER_READ OWNER_EXECUTE EXCLUDE_FROM_ALL)
