import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  transpilePackages: ["@second-site/contracts"],
  reactStrictMode: true,
};

export default nextConfig;
