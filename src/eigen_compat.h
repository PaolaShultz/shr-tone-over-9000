#pragma once

#include <Eigen/Core>

// NeuralAmpModelerCore v0.5.4 uses the Eigen 5 spelling. Debian 12 and 13
// provide Eigen 3.4, where lastN is still exported directly from Eigen.
#if EIGEN_WORLD_VERSION == 3 && EIGEN_MAJOR_VERSION == 4
namespace Eigen::placeholders
{
using Eigen::lastN;
}
#endif
