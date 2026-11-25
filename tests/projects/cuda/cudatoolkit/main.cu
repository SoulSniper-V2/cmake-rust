#include <cuda_runtime.h>
int main() { return cudaGetDeviceCount(nullptr) == cudaErrorInvalidValue ? 1 : 0; }
