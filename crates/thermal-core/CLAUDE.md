# thermal-core

Shared library for the thermal desktop suite.

## What This Provides
- ThermalPalette: all colors as [f32; 4] RGBA constants
- Shared rendering utilities (future: text rendering helpers, wgpu setup)
- Common types used across all thermal components

## Color Palette
Defined in src/palette.rs. Every thermal component imports ThermalPalette from here to ensure consistency.
