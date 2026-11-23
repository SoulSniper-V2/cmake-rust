# Re-including a file rewritten with same-length content and an unchanged
# mtime (coarse filesystem timestamps) must see the new content.
set(f "${CMAKE_CURRENT_BINARY_DIR}/include_rewritten_probe.cmake")
foreach(v 4 8)
  file(WRITE "${f}" "set(probe ${v})\n")
  execute_process(COMMAND touch -d "@1700000000" "${f}")
  include("${f}")
  message("probe=${probe}")
endforeach()
file(REMOVE "${f}")
