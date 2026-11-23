cmake_minimum_required(VERSION 3.21)
project(MultiConfig C)
enable_testing()
add_library(objs OBJECT obj.c)
add_library(stat STATIC stat.c $<TARGET_OBJECTS:objs>)
add_library(shared SHARED shared.c)
set_target_properties(shared PROPERTIES VERSION 1.2.3 SOVERSION 1)
add_executable(main main.c)
target_link_libraries(main PRIVATE stat shared)
target_compile_definitions(main PRIVATE "CFG_$<UPPER_CASE:$<CONFIG>>=1")
add_custom_command(OUTPUT gen_$<CONFIG>.c
  COMMAND ${CMAKE_COMMAND} -E copy ${CMAKE_CURRENT_SOURCE_DIR}/gen.c.in gen_$<CONFIG>.c
  DEPENDS stat COMMENT "Generating for $<CONFIG>")
add_library(genlib STATIC gen_$<CONFIG>.c)
add_custom_target(stamp ALL
  COMMAND ${CMAKE_COMMAND} -E touch stamp-$<CONFIG>.txt
  BYPRODUCTS stamp-$<CONFIG>.txt
  DEPENDS main)
add_custom_target(plain COMMAND ${CMAKE_COMMAND} -E echo plain ${CMAKE_CFG_INTDIR})
add_custom_command(TARGET main POST_BUILD COMMAND ${CMAKE_COMMAND} -E echo built-$<CONFIG>)
add_subdirectory(sub)
add_test(NAME runs COMMAND main)
install(TARGETS main stat shared genlib EXPORT exp
  RUNTIME DESTINATION bin ARCHIVE DESTINATION lib LIBRARY DESTINATION lib)
install(EXPORT exp DESTINATION lib/cmake/mc)
install(FILES $<CONFIG>.txt DESTINATION share OPTIONAL)
export(EXPORT exp FILE mc-targets.cmake)
file(GENERATE OUTPUT info-$<CONFIG>.txt CONTENT "$<CONFIG> $<TARGET_FILE:main>\n")
