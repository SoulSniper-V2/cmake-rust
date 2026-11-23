cmake_minimum_required(VERSION 3.30)
project(modules CXX)
# Only generate; the scan rules need not work.
set(CMAKE_CXX_SCANDEP_SOURCE "")
set(CMAKE_INSTALL_PREFIX /prefix)
add_library(m_one)
target_sources(m_one PUBLIC FILE_SET CXX_MODULES FILES one.cxx)
target_compile_features(m_one PUBLIC cxx_std_20)
add_library(plain plain.cxx)
install(TARGETS m_one plain EXPORT e
  FILE_SET CXX_MODULES DESTINATION src
  CXX_MODULES_BMI DESTINATION "bmi/$<CONFIG>")
install(EXPORT e FILE e-targets.cmake DESTINATION cps NAMESPACE N:: CXX_MODULES_DIRECTORY mods)
install(EXPORT e FILE d-targets.cmake DESTINATION d CXX_MODULES_DIRECTORY mods CONFIGURATIONS Debug)
export(EXPORT e FILE built.cmake CXX_MODULES_DIRECTORY bmods)
