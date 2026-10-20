import requests

# un URL in cui è presente la risorsa
url = "https://cdn8.questosito.com/media/2026/ad456awerA65"

# override dell' header per il referer
headers = {
    "Referer": "https://questosito.com"
    }

response = requests.get(url, headers: headers)

filename = "ilmiovideo.mp4"
with open(filename, "wb") as f:
    f.write(response.content)
    
