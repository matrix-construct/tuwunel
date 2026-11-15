document
	.querySelectorAll('time[data-ts]')
	.forEach(function(el) {
		var t = +el.dataset.ts;
		if(t)
			el.textContent = new Date(t * 1000).toLocaleString();
	});

var collator = new Intl.Collator(undefined, {numeric: true});

function cellTime(cell) {
	var time = cell.querySelector('time[data-ts]');

	return time ? +time.dataset.ts : 0;
}

function compareCells(kind, a, b) {
	if(kind === 'time')
		return cellTime(a) - cellTime(b);

	return collator.compare(a.textContent.trim(), b.textContent.trim());
}

function sortRows(th, order) {
	var body = th.closest('table').tBodies[0];
	var column = th.cellIndex;
	var sign = order === 'ascending' ? 1 : -1;

	Array.from(body.rows)
		.sort(function(a, b) {
			return sign * compareCells(th.dataset.sort, a.cells[column], b.cells[column]);
		})
		.forEach(function(row) {
			body.appendChild(row);
		});

	th.parentElement
		.querySelectorAll('th[aria-sort]')
		.forEach(function(other) {
			other.removeAttribute('aria-sort');
		});

	th.setAttribute('aria-sort', order);
}

document
	.querySelectorAll('th[data-sort]')
	.forEach(function(th) {
		var button = document.createElement('button');

		button.type = 'button';
		button.className = 'sort';
		button.textContent = th.textContent;
		button.addEventListener('click', function() {
			var ascending = th.getAttribute('aria-sort') === 'ascending';

			sortRows(th, ascending ? 'descending' : 'ascending');
		});

		th.replaceChildren(button);
	});
