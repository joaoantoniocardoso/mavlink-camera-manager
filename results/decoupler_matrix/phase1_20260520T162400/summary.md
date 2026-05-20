# UDP decoupler experiment summary

Source: `/home/joaoantoniocardoso/BlueRobotics/mavlink-camera-manager-next/results/decoupler_matrix/phase1_20260520T162400`

Cells: 9 (variants ['appsink', 'b1', 'proxy'], conditions ['idle', 'impair_aggressive', 'impair_mild'])

## Per-cell pairwise arrival deltas (us)

| variant | condition | reps | pair | median | p95 | p99 | max | 95% CI(median) |
|---|---|---:|---|---:|---:|---:|---:|---|
| appsink | idle | 3 | udp-0 - rtsp-0 | -2890.0 | -1912.2 | -1467.2 | -832.0 | [-2918.0, -2857.0] |
| appsink | idle | 3 | webrtc-0 - rtsp-0 | -1159.0 | -760.5 | -642.1 | -437.0 | [-1175.0, -1144.0] |
| appsink | idle | 3 | webrtc-0 - udp-0 | 1715.5 | 2904.2 | 5088.4 | 30574.0 | [1701.0, 1729.0] |
| b1 | idle | 3 | udp-0 - rtsp-0 | -2324.0 | -939.5 | 512.0 | 53626.0 | [-2362.0, -2272.0] |
| b1 | idle | 3 | webrtc-0 - rtsp-0 | -1111.0 | -681.6 | -524.0 | -381.0 | [-1126.0, -1101.0] |
| b1 | idle | 3 | webrtc-0 - udp-0 | 1209.5 | 2244.0 | 4049.6 | 39675.0 | [1182.0, 1231.0] |
| proxy | idle | 3 | udp-0 - rtsp-0 | -2811.5 | -1392.3 | -944.0 | -585.0 | [-2842.0, -2765.0] |
| proxy | idle | 3 | webrtc-0 - rtsp-0 | -1137.5 | -670.0 | -510.6 | -380.0 | [-1152.0, -1122.0] |
| proxy | idle | 3 | webrtc-0 - udp-0 | 1599.0 | 2705.3 | 5098.6 | 50925.0 | [1575.5, 1628.0] |
| appsink | impair_aggressive | 3 | udp-0 - rtsp-0 | -3032.0 | -1689.2 | -1169.9 | -633.0 | [-3081.0, -2978.0] |
| appsink | impair_aggressive | 3 | webrtc-0 - rtsp-0 | 76024.0 | 89699.0 | 108551.0 | 111087.0 | [75229.0, 77093.0] |
| appsink | impair_aggressive | 3 | webrtc-0 - udp-0 | 79046.0 | 93659.0 | 111424.8 | 113874.0 | [78126.0, 80062.0] |
| b1 | impair_aggressive | 3 | udp-0 - rtsp-0 | -2378.0 | -966.7 | 417.5 | 4782.0 | [-2417.0, -2342.0] |
| b1 | impair_aggressive | 3 | webrtc-0 - rtsp-0 | 76523.0 | 111977.3 | 116156.0 | 173851.0 | [75669.0, 78443.0] |
| b1 | impair_aggressive | 3 | webrtc-0 - udp-0 | 79320.0 | 114555.9 | 117146.7 | 175018.0 | [78084.0, 80693.0] |
| proxy | impair_aggressive | 3 | udp-0 - rtsp-0 | -2903.5 | -1308.0 | -914.1 | -593.0 | [-2936.5, -2878.5] |
| proxy | impair_aggressive | 3 | webrtc-0 - rtsp-0 | 74611.0 | 84774.6 | 89351.7 | 95306.0 | [73837.0, 75804.0] |
| proxy | impair_aggressive | 3 | webrtc-0 - udp-0 | 77407.0 | 87123.2 | 92777.3 | 99661.0 | [76092.0, 78515.0] |
| appsink | impair_mild | 3 | udp-0 - rtsp-0 | -2890.0 | -2117.0 | -1684.1 | -733.0 | [-2908.5, -2864.0] |
| appsink | impair_mild | 3 | webrtc-0 - rtsp-0 | 27214.0 | 30080.5 | 30391.7 | 30509.0 | [26556.0, 28691.0] |
| appsink | impair_mild | 3 | webrtc-0 - udp-0 | 30572.0 | 32895.5 | 33495.5 | 33740.0 | [29711.0, 31735.0] |
| b1 | impair_mild | 3 | udp-0 - rtsp-0 | -2521.0 | -1099.0 | 430.2 | 17602.0 | [-2573.0, -2478.0] |
| b1 | impair_mild | 3 | webrtc-0 - rtsp-0 | 27155.0 | 31772.3 | 33257.6 | 34213.0 | [26578.0, 27664.0] |
| b1 | impair_mild | 3 | webrtc-0 - udp-0 | 29725.0 | 34551.9 | 35904.5 | 36361.0 | [29100.0, 30381.0] |
| proxy | impair_mild | 3 | udp-0 - rtsp-0 | -2897.5 | -1361.2 | -934.1 | -610.0 | [-2927.0, -2854.0] |
| proxy | impair_mild | 3 | webrtc-0 - rtsp-0 | 25999.0 | 29962.2 | 30509.6 | 30626.0 | [24047.0, 27253.0] |
| proxy | impair_mild | 3 | webrtc-0 - udp-0 | 28721.0 | 32610.4 | 32771.5 | 32785.0 | [27525.5, 30121.0] |

## CPU and drops

| variant | condition | reps | cpu_mcm_mean(%) | cpu_mcm_p95(%) | sys_user_mean(%) | drops_max | drops_sum | windows_w/drops |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| appsink | idle | 3 | 5.85 | 7.05 | 7.83 | 0 | 0 | 0 |
| b1 | idle | 3 | 4.46 | 5.31 | 7.09 | 0 | 0 | 0 |
| proxy | idle | 3 | 4.42 | 5.19 | 7.54 | 0 | 0 | 0 |
| appsink | impair_aggressive | 3 | 6.01 | 7.11 | 8.58 | 0 | 0 | 0 |
| b1 | impair_aggressive | 3 | 4.65 | 5.36 | 7.91 | 0 | 0 | 0 |
| proxy | impair_aggressive | 3 | 4.57 | 5.35 | 7.92 | 0 | 0 | 0 |
| appsink | impair_mild | 3 | 5.98 | 7.03 | 8.36 | 0 | 0 | 0 |
| b1 | impair_mild | 3 | 4.47 | 5.30 | 8.08 | 0 | 0 | 0 |
| proxy | impair_mild | 3 | 4.56 | 5.36 | 7.94 | 0 | 0 | 0 |

## Cross-variant comparison (within condition)

Best variant per (condition, pair) is the one with the lowest median pairwise arrival delta.
Mann-Whitney U test (Bonferroni-corrected across variants). Cliff's delta is signed: >0 means the
comparison variant has *larger* (worse) deltas than the best.

| condition | pair | best | vs | best_median(us) | vs_median(us) | p_bonf | Cliff's d | effect |
|---|---|---|---|---:|---:|---:|---:|---|
| idle | udp-0_minus_rtsp-0_us | appsink | b1 | -2890.0 | -2324.0 | 1.718e-203 | +0.454 | medium |
| idle | udp-0_minus_rtsp-0_us | appsink | proxy | -2890.0 | -2811.5 | 3.886e-26 | +0.158 | small |
| idle | webrtc-0_minus_rtsp-0_us | appsink | b1 | -1159.0 | -1111.0 | 3.206e-15 | +0.119 | negligible |
| idle | webrtc-0_minus_rtsp-0_us | appsink | proxy | -1159.0 | -1137.5 | 2.278e-07 | +0.079 | negligible |
| idle | webrtc-0_minus_udp-0_us | b1 | appsink | 1209.5 | 1715.5 | 0 | +0.579 | large |
| idle | webrtc-0_minus_udp-0_us | b1 | proxy | 1209.5 | 1599.0 | 2.231e-134 | +0.368 | medium |
| impair_aggressive | udp-0_minus_rtsp-0_us | appsink | b1 | -3032.0 | -2378.0 | 3.537e-181 | +0.428 | medium |
| impair_aggressive | udp-0_minus_rtsp-0_us | appsink | proxy | -3032.0 | -2903.5 | 1.714e-16 | +0.124 | negligible |
| impair_aggressive | webrtc-0_minus_rtsp-0_us | proxy | appsink | 74611.0 | 76024.0 | 0.02904 | +0.139 | negligible |
| impair_aggressive | webrtc-0_minus_rtsp-0_us | proxy | b1 | 74611.0 | 76523.0 | 0.002531 | +0.217 | small |
| impair_aggressive | webrtc-0_minus_udp-0_us | proxy | appsink | 77407.0 | 79046.0 | 0.005141 | +0.171 | small |
| impair_aggressive | webrtc-0_minus_udp-0_us | proxy | b1 | 77407.0 | 79320.0 | 0.007978 | +0.194 | small |
| impair_mild | udp-0_minus_rtsp-0_us | proxy | appsink | -2897.5 | -2890.0 | 2.127e-19 | -0.135 | negligible |
| impair_mild | udp-0_minus_rtsp-0_us | proxy | b1 | -2897.5 | -2521.0 | 2.426e-36 | +0.188 | small |
| impair_mild | webrtc-0_minus_rtsp-0_us | proxy | appsink | 25999.0 | 27214.0 | 0.08859 | +0.306 | small |
| impair_mild | webrtc-0_minus_rtsp-0_us | proxy | b1 | 25999.0 | 27155.0 | 0.01966 | +0.309 | small |
| impair_mild | webrtc-0_minus_udp-0_us | proxy | appsink | 28721.0 | 30572.0 | 0.02399 | +0.382 | medium |
| impair_mild | webrtc-0_minus_udp-0_us | proxy | b1 | 28721.0 | 29725.0 | 0.05744 | +0.262 | small |
