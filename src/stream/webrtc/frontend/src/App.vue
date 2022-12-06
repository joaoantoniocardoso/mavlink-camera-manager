<script lang="ts">
import Vue from "vue";

const ip = window.location.hostname.toString();
// const ip = "blueos.local";

export default {
  data() {
    return {
      message: "Hello World!",
    };
  },
  methods: {
    reverseMessage() {
      this.message = this.message.split("").reverse().join("");
    },
    notify() {
      alert("navigation was prevented.");
    },
  },
};
</script>

<template>
  <header>
    <h1>Mavlink Camera Manager Development UI</h1>
  </header>
  <br />

  <main>
    <div style="display: flex; column-gap: 1em">

    </div>
  </main>
</template>
