// Runs in a worker so the tick keeps its pace in a hidden tab. The page tells it how often.
let timer;
onmessage = (e) => {
  clearInterval(timer);
  timer = setInterval(() => postMessage(0), e.data);
};
