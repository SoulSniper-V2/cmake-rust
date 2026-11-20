file(WRITE source value)
file(INSTALL DESTINATION prefix TYPE FILE FILES source MESSAGE_NEVER)
message("${CMAKE_INSTALL_MANIFEST_FILES}")
message("[${out}]")
