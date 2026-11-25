# CRLF line endings in an included list file become LF (cmListFileLexer file
# input), including inside quoted and bracket arguments; a lone CR stays.
file(WRITE crlf.cmake "if(\"a\r\nb\" STREQUAL \"a\nb\")\r\n  message(\"quoted ok\")\r\nendif()\r\nset(x [[l1\r\nl2]])\r\nstring(LENGTH \"\${x}\" n)\r\nmessage(\"bracket length \${n}\")\r\nset(y \"a\rb\")\r\nstring(LENGTH \"\${y}\" m)\r\nmessage(\"cr length \${m}\")\r\n")
include(${CMAKE_CURRENT_BINARY_DIR}/crlf.cmake OPTIONAL RESULT_VARIABLE r)
if(NOT r)
  include(crlf.cmake)
endif()
