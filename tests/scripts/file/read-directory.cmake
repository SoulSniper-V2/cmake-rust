# Reading a directory fails std::getline without EOF: one "\n" comes back.
file(MAKE_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}/read-directory-d")
file(READ "${CMAKE_CURRENT_BINARY_DIR}/read-directory-d" text)
message("text=[${text}]")
file(READ "${CMAKE_CURRENT_BINARY_DIR}/read-directory-d" hex HEX)
message("hex=[${hex}]")
file(REMOVE_RECURSE "${CMAKE_CURRENT_BINARY_DIR}/read-directory-d")
