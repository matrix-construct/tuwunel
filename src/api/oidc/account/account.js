document
	.querySelectorAll('time[data-ts]')
	.forEach(function(el) {
		var t = +el.dataset.ts;
		if(t)
			el.textContent = new Date(t * 1000).toLocaleString();
	});
