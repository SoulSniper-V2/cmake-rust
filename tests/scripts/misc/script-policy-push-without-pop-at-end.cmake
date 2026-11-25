# The -P script's own list-file snapshot rejects a still-open policy scope.
block(SCOPE_FOR POLICIES)
  cmake_policy(PUSH)
endblock()
message("after")
