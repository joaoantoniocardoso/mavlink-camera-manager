# UDP decoupler experiment summary

Source: `/home/joaoantoniocardoso/BlueRobotics/mavlink-camera-manager-next/results/decoupler_matrix/webrtc_refactor_20260520T200938`

Cells: 3 (variants ['proxy'], conditions ['idle', 'impair_aggressive', 'impair_mild'])

## Per-cell pairwise arrival deltas (us)

| variant | condition | reps | pair | median | p95 | p99 | max | 95% CI(median) |
|---|---|---:|---|---:|---:|---:|---:|---|
| proxy | idle | 3 | udp-0 - rtsp-0 | -1738.5 | -1078.3 | -827.4 | -474.0 | [-1762.0, -1715.0] |
| proxy | idle | 3 | webrtc-0 - rtsp-0 | -202.0 | 491.0 | 650.9 | 1450.0 | [-224.0, -185.5] |
| proxy | idle | 3 | webrtc-0 - udp-0 | 1465.5 | 2317.7 | 2668.4 | 5645.0 | [1448.5, 1484.0] |
| proxy | impair_aggressive | 3 | udp-0 - rtsp-0 | -1635.0 | -1063.4 | -887.4 | -525.0 | [-1659.0, -1613.0] |
| proxy | impair_aggressive | 3 | webrtc-0 - rtsp-0 | 77331.5 | 85759.2 | 93708.4 | 99699.0 | [74273.0, 80310.0] |
| proxy | impair_aggressive | 3 | webrtc-0 - udp-0 | 79746.0 | 87788.5 | 95081.8 | 101085.0 | [76704.0, 81690.0] |
| proxy | impair_mild | 3 | udp-0 - rtsp-0 | -1704.0 | -1075.0 | -923.1 | -576.0 | [-1728.0, -1677.0] |
| proxy | impair_mild | 3 | webrtc-0 - rtsp-0 | 25577.5 | 26583.2 | 26704.7 | 26735.0 | [24269.5, 26431.5] |
| proxy | impair_mild | 3 | webrtc-0 - udp-0 | 26924.5 | 27507.0 | 27595.0 | 27617.0 | [26298.0, 27397.0] |

## CPU and drops

| variant | condition | reps | cpu_mcm_mean(%) | cpu_mcm_p95(%) | sys_user_mean(%) | drops_max | drops_sum | windows_w/drops |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| proxy | idle | 3 | 4.54 | 5.31 | 7.21 | 0 | 0 | 0 |
| proxy | impair_aggressive | 3 | 4.67 | 5.37 | 7.98 | 0 | 0 | 0 |
| proxy | impair_mild | 3 | 4.66 | 5.37 | 7.92 | 0 | 0 | 0 |
