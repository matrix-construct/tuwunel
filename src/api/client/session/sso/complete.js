if (window.onAuthDone) {
	window.onAuthDone()
} else if (window.opener && window.opener.postMessage) {
	window.opener.postMessage("authDone", "*")
}
