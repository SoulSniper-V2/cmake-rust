# Verbatim from Kitware CMake 4.1.0 Tests/RunCMake/string/UuidMissingTypeValue.cmake
# (BSD-3-Clause, Kitware Inc.) - differential coverage of upstream test content.
set(UUID_DNS_NAMESPACE 6ba7b810-9dad-11d1-80b4-00c04fd430c8)

string(UUID WWW_EXAMPLE_COM_MD5_UUID
  NAMESPACE ${UUID_DNS_NAMESPACE} NAME www.example.com TYPE)
