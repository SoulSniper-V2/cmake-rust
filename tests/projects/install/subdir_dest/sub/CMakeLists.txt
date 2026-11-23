# Destinations and paths are evaluated in this directory: an imported target
# that is not visible at the top, and a directory-scoped policy.
add_library(imp SHARED IMPORTED)
set_target_properties(imp PROPERTIES IMPORTED_LOCATION /opt/imp/libimp.so WHERE share/imp)
add_library(o OBJECT o.c)
install(FILES "$<TARGET_FILE:imp>" DESTINATION "$<TARGET_PROPERTY:imp,WHERE>" RENAME "$<TARGET_FILE_NAME:imp>.1")
install(FILES "$<TARGET_OBJECTS:o>" DESTINATION /abs/obj)
install(DIRECTORY "$<TARGET_PROPERTY:imp,WHERE>/" DESTINATION "d/$<TARGET_FILE_NAME:imp>")
install(CODE "message([[$<TARGET_FILE:imp>]])")
install(TARGETS o OBJECTS DESTINATION "$<TARGET_PROPERTY:imp,WHERE>")
install(IMPORTED_RUNTIME_ARTIFACTS imp RUNTIME_DEPENDENCY_SET deps LIBRARY DESTINATION "l/$<TARGET_FILE_NAME:imp>")
add_library(real SHARED o.c)
target_link_libraries(real PRIVATE imp)
install(TARGETS real RUNTIME_DEPENDENCY_SET deps)
install(RUNTIME_DEPENDENCY_SET deps
  DIRECTORIES "$<TARGET_FILE_DIR:imp>"
  LIBRARY DESTINATION "/abs/$<TARGET_FILE_NAME:imp>" CONFIGURATIONS Debug Release)
