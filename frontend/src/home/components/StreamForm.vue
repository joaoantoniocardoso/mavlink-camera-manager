<template>
  <form>
    <p>
      <label>Name: </label>
      <input
        name="name"
        type="text"
        autocomplete="off"
        v-model="stream_setting.name"
      />
    </p>

    <div>
      <label>Capture format: </label>
      <select
        v-model="stream_setting.configuration.source_encode"
        :disabled="stream_setting.source == 'Redirect'"
      >
        <option
          v-for="encode in stream_options.encoders"
          :key="encode"
          :value="encode"
        >
          {{ encode }}
        </option>
      </select>
    </div>
    <div>
      <label>Output format: </label>
      <select
        v-model="stream_setting.configuration.encode"
        :disabled="stream_setting.source == 'Redirect'"
      >
        <option
          v-for="encode in sinkEncoders"
          :key="encode"
          :value="encode"
        >
          {{ encode }}
        </option>
      </select>
    </div>
    <div>
      <label>Size: </label>
      <select
        v-model="stream_setting.configuration.size"
        :disabled="stream_setting.source == 'Redirect'"
      >
        <option
          v-for="size in stream_options.sizes"
          :key="size.width + 'x' + size.height"
          :value="{ width: size.width, height: size.height }"
        >
          {{ size.width }} x {{ size.height }}
        </option>
      </select>
    </div>
    <div>
      <label>Bit depth: </label>
      <select
        v-model="stream_setting.configuration.bit_depth"
        :disabled="stream_setting.source == 'Redirect' || !bitDepthAvailable"
      >
        <option v-if="!bitDepthAvailable" :value="undefined">
          Not available
        </option>
        <option v-else :value="undefined">Auto</option>
        <option
          v-for="depth in stream_options.depths"
          :key="depth.bit_depth"
          :value="depth.bit_depth"
        >
          {{ depth.bit_depth }}-bit
        </option>
      </select>
    </div>
    <div>
      <label>FPS: </label>
      <select
        v-model="stream_setting.configuration.interval"
        :disabled="stream_setting.source == 'Redirect'"
      >
        <option
          v-for="interval in stream_options.intervals"
          :key="interval.denominator + '/' + interval.numerator"
          :value="interval"
        >
          {{ +(interval.denominator / interval.numerator).toFixed(2) }}
        </option>
      </select>
    </div>
    <div>
      <label>Thermal: </label>
      <input
        type="checkbox"
        v-model="stream_setting.extended_configuration.thermal"
      />
    </div>
    <div>
      <label>Disable Mavlink: </label>
      <input
        type="checkbox"
        v-model="stream_setting.extended_configuration.disable_mavlink"
      />
    </div>
    <div>
      <label>Disable Zenoh: </label>
      <input
        type="checkbox"
        v-model="stream_setting.extended_configuration.disable_zenoh"
      />
    </div>
    <div>
      <label>Disable Thumbnails: </label>
      <input
        type="checkbox"
        v-model="stream_setting.extended_configuration.disable_thumbnails"
      />
    </div>
    <div>
      <label>Disable Lazy: </label>
      <input
        type="checkbox"
        v-model="stream_setting.extended_configuration.disable_lazy"
      />
    </div>
    <div>
      <label>Disable Recording: </label>
      <input
        type="checkbox"
        v-model="stream_setting.extended_configuration.disable_recording"
      />
    </div>

    <p>
      <label>Endpoints: </label>
      <input
        type="text"
        autocomplete="off"
        placeholder="udp://0.0.0.0:5600"
        v-model="stream_setting.endpoints"
      />
    </p>
    <button type="button" @click="$emit('onconfigure', stream_setting)">
      Configure stream
    </button>
  </form>
</template>

<script lang="ts">
import { defineComponent } from "vue";

export default defineComponent({
  name: "StreamForm",
  props: {
    device: {
      type: Object,
      required: true,
    },
    streams: {
      type: Object,
      required: true,
    },
  },
  emits: ["onconfigure"],
  mounted() {
    this.stream_options.encoders = this.device.formats.map((format: any) =>
      this.encodeToStr(format.encode)
    );
  },
  watch: {
    streams: {
      handler(streams: any[]) {
        this.stream = streams.filter(
          (stream: any) =>
            (stream.video_and_stream.video_source.Local &&
              stream.video_and_stream.video_source.Local.device_path ==
                this.device.source) ||
            (stream.video_and_stream.video_source.Gst &&
              stream.video_and_stream.video_source.Gst.source.Fake ==
                this.device.source)
        )[0];
        if (!this.stream) {
          return;
        }

        switch (
          this.stream.video_and_stream.stream_information.configuration.type
        ) {
          case "redirect":
            break;
          default: {
            const configuration =
              this.stream.video_and_stream.stream_information.configuration;
            this.stream_setting.configuration.source_encode =
              configuration.source_encode ?? configuration.encode;
            this.stream_setting.configuration.encode = configuration.encode;
            this.stream_setting.configuration.size = {
              height:
                this.stream.video_and_stream.stream_information.configuration
                  .height,
              width:
                this.stream.video_and_stream.stream_information.configuration
                  .width,
            };
            this.stream_setting.configuration.interval =
              this.stream.video_and_stream.stream_information.configuration.frame_interval;
            this.stream_setting.configuration.bit_depth =
              this.stream.video_and_stream.stream_information.configuration.bit_depth;
          }
        }

        this.stream_setting.endpoints = this.stream.video_and_stream
          .stream_information.endpoints
          ? this.stream.video_and_stream.stream_information.endpoints.join(", ")
          : "";
        this.stream_setting.extended_configuration.thermal = Boolean(
          this.stream.video_and_stream.stream_information
            .extended_configuration?.thermal
        );
        this.stream_setting.extended_configuration.disable_mavlink = Boolean(
          this.stream.video_and_stream.stream_information
            .extended_configuration?.disable_mavlink
        );
        this.stream_setting.extended_configuration.disable_zenoh = Boolean(
          this.stream.video_and_stream.stream_information
            .extended_configuration?.disable_zenoh
        );
        this.stream_setting.extended_configuration.disable_thumbnails = Boolean(
          this.stream.video_and_stream.stream_information
            .extended_configuration?.disable_thumbnails
        );
        this.stream_setting.extended_configuration.disable_lazy = Boolean(
          this.stream.video_and_stream.stream_information
            .extended_configuration?.disable_lazy
        );
        this.stream_setting.extended_configuration.disable_recording = Boolean(
          this.stream.video_and_stream.stream_information
            .extended_configuration?.disable_recording
        );
      },
      deep: true,
    },
    stream_setting: {
      handler(stream_setting: any) {
        console.log(JSON.stringify(stream_setting, undefined, 2));

        switch (stream_setting.configuration.type) {
          case "redirect":
            break;
          default: {
            this.stream_options.encoders = this.device.formats.map(
              (format: any) => this.encodeToStr(format.encode)
            );

            const sink_encoders = this.sinkEncoders;
            if (!stream_setting.configuration.encode && stream_setting.configuration.source_encode) {
              this.stream_setting.configuration.encode =
                stream_setting.configuration.source_encode;
            } else if (
              stream_setting.configuration.encode &&
              !sink_encoders.includes(stream_setting.configuration.encode)
            ) {
              this.stream_setting.configuration.encode =
                stream_setting.configuration.source_encode;
            }

            this.stream_options.sizes = this.device.formats
              .filter(
                (format: any) =>
                  this.encodeToStr(format.encode) ==
                  stream_setting.configuration.source_encode
              )
              .map((format: any) => format.sizes)[0]
              // Sort width by preference
              ?.sort(
                (size1: any, size2: any) =>
                  10 * size2.width +
                  size2.height -
                  (10 * size1.width + size1.height)
              );

            console.log(this.stream_options.sizes);

            const chosen_size = stream_setting.configuration.size;
            if (chosen_size == undefined) {
              return;
            }

            const chosen = this.stream_options.sizes?.filter(
              (size: any) =>
                size.width == chosen_size.width &&
                size.height == chosen_size.height
            )[0];
            this.stream_options.depths = chosen?.depths ?? [];
            const chosen_bit_depth = stream_setting.configuration.bit_depth;
            if (
              chosen_bit_depth != null &&
              !this.stream_options.depths.some(
                (depth: any) => depth.bit_depth == chosen_bit_depth
              )
            ) {
              this.stream_setting.configuration.bit_depth = undefined;
            }
            const selected_depth =
              this.stream_options.depths.find(
                (depth: any) =>
                  depth.bit_depth ==
                  this.stream_setting.configuration.bit_depth
              ) ??
              this.stream_options.depths.find(
                (depth: any) => depth.bit_depth == 10
              ) ??
              this.stream_options.depths[0];
            this.stream_options.intervals =
              selected_depth?.intervals ?? chosen?.intervals;
            const chosen_interval = stream_setting.configuration.interval;
            if (
              chosen_interval != null &&
              Array.isArray(this.stream_options.intervals) &&
              !this.stream_options.intervals.some(
                (interval: any) =>
                  interval.numerator == chosen_interval.numerator &&
                  interval.denominator == chosen_interval.denominator
              )
            ) {
              this.stream_setting.configuration.interval =
                this.stream_options.intervals[0];
            }
          }
        }
      },
      deep: true,
    },
  },
  computed: {
    bitDepthAvailable(): boolean {
      return (
        Array.isArray(this.stream_options.depths) &&
        this.stream_options.depths.length > 0
      );
    },
    sinkEncoders(): string[] {
      const source_encode = this.stream_setting.configuration.source_encode;
      if (!source_encode) {
        return [];
      }
      const sink_encoders = [source_encode];
      if (
        ["NV12", "YUYV", "RGB"].includes(source_encode) &&
        !sink_encoders.includes("H264")
      ) {
        sink_encoders.push("H264");
      }
      return sink_encoders;
    },
  },
  methods: {
    encodeToStr(encode: any): string {
      return typeof encode == "object"
        ? (Object.values(encode)[0] as string)
        : encode;
    },
  },
  data() {
    return {
      stream_setting: {
        name: this.device.source + " - " + this.device.name,
        source: this.device.source,
        endpoints: undefined as string | undefined,
        configuration: {
          source_encode: undefined as string | undefined,
          encode: undefined as string | undefined,
          size: undefined as any,
          interval: undefined as any,
          bit_depth: undefined as number | undefined,
        },
        extended_configuration: {
          thermal: undefined as boolean | undefined,
          disable_mavlink: undefined as boolean | undefined,
          disable_zenoh: undefined as boolean | undefined,
          disable_thumbnails: undefined as boolean | undefined,
          disable_lazy: undefined as boolean | undefined,
          disable_recording: undefined as boolean | undefined,
        },
      },
      stream_options: {
        encoders: undefined as string[] | undefined,
        sizes: undefined as any[] | undefined,
        intervals: undefined as any[] | undefined,
        depths: [] as any[],
      },
      stream: undefined as any,
    };
  },
});
</script>

<style scoped>
select:disabled {
  color: #888;
  background-color: #e8e8e8;
  cursor: not-allowed;
}
</style>
