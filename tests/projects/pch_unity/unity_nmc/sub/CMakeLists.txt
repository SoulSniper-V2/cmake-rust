file(WRITE ${CMAKE_CURRENT_BINARY_DIR}/gen.c "int gen;\n")
add_library(sub STATIC s.c ../a.c ${CMAKE_CURRENT_BINARY_DIR}/gen.c ${CMAKE_BINARY_DIR}/gen/g.c)
set_target_properties(sub PROPERTIES UNITY_BUILD_RELOCATABLE ON UNITY_BUILD_UNIQUE_ID UID)
