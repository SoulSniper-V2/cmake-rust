cmake_minimum_required(VERSION 3.20)
project(mki NONE)
# Installing only an INTERFACE library registers no install component, so
# list_install_components reports "Only default component available".
add_library(iface INTERFACE)
install(TARGETS iface EXPORT ex)
install(EXPORT ex DESTINATION lib/cmake/ex)
