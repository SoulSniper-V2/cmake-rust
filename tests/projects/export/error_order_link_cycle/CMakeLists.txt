cmake_minimum_required(VERSION 3.20)
project(eo C)
# The link-group cycle is found while the local generators generate their
# targets, which cmGlobalGenerator::Generate does before it writes the
# build export sets, so export()'s error comes after it.
add_library(a STATIC a.c)
add_library(b STATIC b.c)
target_link_libraries(a PUBLIC b)
target_link_libraries(b PUBLIC a)
add_executable(app main.c)
target_link_libraries(app PRIVATE a "$<LINK_GROUP:RESCAN,b>")
install(TARGETS a EXPORT ex ARCHIVE DESTINATION lib)
install(EXPORT ex DESTINATION lib/cmake)
export(EXPORT ex FILE exTargets.cmake)
