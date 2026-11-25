# COPY_ON_ERROR falls back to kwsys CopyFileAlways, which creates the
# destination directory before it opens the source: here that directory
# is the (missing) source itself, so an empty file is "copied" from it.
set(w "${CMAKE_CURRENT_BINARY_DIR}/create-link-copy-on-error")
file(REMOVE_RECURSE "${w}")
file(MAKE_DIRECTORY "${w}")
file(CREATE_LINK "${w}/d" "${w}/d/sub/f.txt" COPY_ON_ERROR SYMBOLIC RESULT r)
file(SIZE "${w}/d/sub/f.txt" s)
message("r=[${r}] size=[${s}]")
file(CREATE_LINK "${w}/nofile" "${w}/x/y/g.txt" COPY_ON_ERROR RESULT r2)
message("r2=[${r2}]")
file(REMOVE_RECURSE "${w}")
