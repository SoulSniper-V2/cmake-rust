cmake_minimum_required(VERSION 3.20)
project(mk C)
# Unix Makefiles specifics: the codemodel link line uses the base link-line
# computer relative to the target's directory, a versioned library's
# symlink rule touches its top-relative path, and list_install_components
# reports the registered component set ("Only default component available"
# when no install() named a component).
add_subdirectory(sub)
add_executable(app main.c)
target_link_libraries(app PRIVATE sh)
