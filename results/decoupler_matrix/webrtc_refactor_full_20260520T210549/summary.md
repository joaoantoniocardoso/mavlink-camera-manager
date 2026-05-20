# UDP decoupler experiment summary

Source: `/home/joaoantoniocardoso/BlueRobotics/mavlink-camera-manager-next/results/decoupler_matrix/webrtc_refactor_full_20260520T210549`

Cells: 9 (variants ['appsink', 'b1', 'proxy'], conditions ['idle', 'impair_aggressive', 'impair_mild'])

## Per-cell pairwise arrival deltas (us)

| variant | condition | reps | pair | median | p95 | p99 | max | 95% CI(median) |
|---|---|---:|---|---:|---:|---:|---:|---|
| appsink | idle | 3 | udp-0 - rtsp-0 | -1762.0 | -1102.0 | -915.7 | -602.0 | [-1797.0, -1735.0] |
| appsink | idle | 3 | webrtc-0 - rtsp-0 | -119.0 | 634.6 | 772.6 | 1127.0 | [-142.0, -96.0] |
| appsink | idle | 3 | webrtc-0 - udp-0 | 1634.0 | 2375.0 | 2680.5 | 6459.0 | [1618.0, 1652.0] |
| b1 | idle | 3 | udp-0 - rtsp-0 | -1005.0 | 585.8 | 1216.7 | 4336.0 | [-1041.0, -964.0] |
| b1 | idle | 3 | webrtc-0 - rtsp-0 | -642.0 | 225.6 | 701.0 | 1570.0 | [-664.0, -618.0] |
| b1 | idle | 3 | webrtc-0 - udp-0 | 389.0 | 1363.6 | 1724.8 | 2716.0 | [351.0, 421.0] |
| proxy | idle | 3 | udp-0 - rtsp-0 | -1709.5 | -1141.0 | -910.4 | -536.0 | [-1738.0, -1685.0] |
| proxy | idle | 3 | webrtc-0 - rtsp-0 | -173.0 | 433.0 | 654.6 | 1138.0 | [-203.0, -131.0] |
| proxy | idle | 3 | webrtc-0 - udp-0 | 1477.0 | 2200.3 | 2643.4 | 8156.0 | [1467.0, 1487.0] |
| appsink | impair_aggressive | 3 | udp-0 - rtsp-0 | -1800.5 | -1084.3 | -849.6 | -508.0 | [-1829.0, -1768.0] |
| appsink | impair_aggressive | 3 | webrtc-0 - rtsp-0 | 77360.5 | 88526.9 | 93592.5 | 112482.0 | [76150.9, 78075.0] |
| appsink | impair_aggressive | 3 | webrtc-0 - udp-0 | 79023.5 | 90608.2 | 95363.5 | 113764.0 | [78377.5, 79879.5] |
| b1 | impair_aggressive | 3 | udp-0 - rtsp-0 | -1350.5 | 699.3 | 1138.7 | 8646.0 | [-1389.0, -1313.5] |
| b1 | impair_aggressive | 3 | webrtc-0 - rtsp-0 | 77269.0 | 89767.4 | 99174.0 | 105207.0 | [76423.0, 78835.0] |
| b1 | impair_aggressive | 3 | webrtc-0 - udp-0 | 79026.0 | 91231.2 | 99394.7 | 106335.0 | [76828.0, 80000.0] |
| proxy | impair_aggressive | 3 | udp-0 - rtsp-0 | -1669.0 | -1149.4 | -1058.3 | -634.0 | [-1693.0, -1648.0] |
| proxy | impair_aggressive | 3 | webrtc-0 - rtsp-0 | 77200.0 | 91952.2 | 99918.9 | 103045.0 | [76197.0, 77792.0] |
| proxy | impair_aggressive | 3 | webrtc-0 - udp-0 | 78826.0 | 93293.2 | 101438.4 | 104308.0 | [78039.0, 79431.0] |
| appsink | impair_mild | 3 | udp-0 - rtsp-0 | -1754.0 | -1023.3 | -829.1 | -506.0 | [-1798.0, -1719.0] |
| appsink | impair_mild | 3 | webrtc-0 - rtsp-0 | 28515.0 | 31802.5 | 35809.5 | 38238.0 | [26588.0, 29230.0] |
| appsink | impair_mild | 3 | webrtc-0 - udp-0 | 30049.0 | 33613.0 | 37249.0 | 39634.0 | [28707.0, 31094.0] |
| b1 | impair_mild | 3 | udp-0 - rtsp-0 | -1319.0 | 697.3 | 1213.2 | 5391.0 | [-1348.0, -1283.0] |
| b1 | impair_mild | 3 | webrtc-0 - rtsp-0 | 27009.0 | 31139.0 | 32454.4 | 33158.0 | [26380.0, 27870.0] |
| b1 | impair_mild | 3 | webrtc-0 - udp-0 | 28963.0 | 32775.6 | 34202.4 | 34788.0 | [28083.0, 29550.0] |
| proxy | impair_mild | 3 | udp-0 - rtsp-0 | -1657.0 | -1128.3 | -1038.0 | -674.0 | [-1678.0, -1636.0] |
| proxy | impair_mild | 3 | webrtc-0 - rtsp-0 | 24123.0 | 25373.6 | 25483.5 | 25511.0 | [19466.0, 25511.0] |
| proxy | impair_mild | 3 | webrtc-0 - udp-0 | 25963.0 | 27353.6 | 27501.1 | 27538.0 | [20186.0, 27538.0] |

## CPU and drops

| variant | condition | reps | cpu_mcm_mean(%) | cpu_mcm_p95(%) | sys_user_mean(%) | drops_max | drops_sum | windows_w/drops |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| appsink | idle | 3 | 6.16 | 7.27 | 7.65 | 0 | 0 | 0 |
| b1 | idle | 3 | 4.59 | 5.31 | 7.43 | 0 | 0 | 0 |
| proxy | idle | 3 | 4.52 | 5.30 | 7.13 | 0 | 0 | 0 |
| appsink | impair_aggressive | 3 | 6.36 | 7.49 | 8.46 | 0 | 0 | 0 |
| b1 | impair_aggressive | 3 | 4.71 | 5.42 | 7.85 | 0 | 0 | 0 |
| proxy | impair_aggressive | 3 | 4.68 | 5.36 | 7.79 | 0 | 0 | 0 |
| appsink | impair_mild | 3 | 5.96 | 7.13 | 8.30 | 0 | 0 | 0 |
| b1 | impair_mild | 3 | 4.68 | 5.41 | 7.80 | 0 | 0 | 0 |
| proxy | impair_mild | 3 | 4.62 | 5.35 | 8.04 | 0 | 0 | 0 |

## Cross-variant comparison (within condition)

Best variant per (condition, pair) is the one with the lowest median pairwise arrival delta.
Mann-Whitney U test (Bonferroni-corrected across variants). Cliff's delta is signed: >0 means the
comparison variant has *larger* (worse) deltas than the best.

| condition | pair | best | vs | best_median(us) | vs_median(us) | p_bonf | Cliff's d | effect |
|---|---|---|---|---:|---:|---:|---:|---|
| idle | udp-0_minus_rtsp-0_us | appsink | b1 | -1762.0 | -1005.0 | 0 | +0.586 | large |
| idle | udp-0_minus_rtsp-0_us | appsink | proxy | -1762.0 | -1709.5 | 1.026e-06 | +0.075 | negligible |
| idle | webrtc-0_minus_rtsp-0_us | b1 | appsink | -642.0 | -119.0 | 4.545e-103 | +0.321 | small |
| idle | webrtc-0_minus_rtsp-0_us | b1 | proxy | -642.0 | -173.0 | 3.348e-82 | +0.286 | small |
| idle | webrtc-0_minus_udp-0_us | b1 | appsink | 389.0 | 1634.0 | 0 | +0.915 | large |
| idle | webrtc-0_minus_udp-0_us | b1 | proxy | 389.0 | 1477.0 | 0 | +0.875 | large |
| impair_aggressive | udp-0_minus_rtsp-0_us | appsink | b1 | -1800.5 | -1350.5 | 5.838e-177 | +0.423 | medium |
| impair_aggressive | udp-0_minus_rtsp-0_us | appsink | proxy | -1800.5 | -1669.0 | 2.292e-16 | +0.123 | negligible |
| impair_aggressive | webrtc-0_minus_rtsp-0_us | proxy | appsink | 77200.0 | 77360.5 | 1 | -0.016 | negligible |
| impair_aggressive | webrtc-0_minus_rtsp-0_us | proxy | b1 | 77200.0 | 77269.0 | 1 | +0.016 | negligible |
| impair_aggressive | webrtc-0_minus_udp-0_us | proxy | appsink | 78826.0 | 79023.5 | 1 | -0.016 | negligible |
| impair_aggressive | webrtc-0_minus_udp-0_us | proxy | b1 | 78826.0 | 79026.0 | 0.8314 | -0.052 | negligible |
| impair_mild | udp-0_minus_rtsp-0_us | appsink | b1 | -1754.0 | -1319.0 | 7.561e-163 | +0.405 | medium |
| impair_mild | udp-0_minus_rtsp-0_us | appsink | proxy | -1754.0 | -1657.0 | 2.579e-08 | +0.085 | negligible |
| impair_mild | webrtc-0_minus_rtsp-0_us | proxy | appsink | 24123.0 | 28515.0 | 0.00604 | +0.824 | large |
| impair_mild | webrtc-0_minus_rtsp-0_us | proxy | b1 | 24123.0 | 27009.0 | 0.008137 | +0.811 | large |
| impair_mild | webrtc-0_minus_udp-0_us | proxy | appsink | 25963.0 | 30049.0 | 0.001419 | +0.892 | large |
| impair_mild | webrtc-0_minus_udp-0_us | proxy | b1 | 25963.0 | 28963.0 | 0.01455 | +0.822 | large |
